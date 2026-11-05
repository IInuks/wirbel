//! Runs the compositor without a backend and talks to it as a real Wayland client. Input is
//! injected where a backend would deliver it, output is rendered with pixman.

use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    os::{fd::AsFd, unix::net::UnixStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};

use smithay::backend::input::{Axis, AxisSource};
use smithay::{
    backend::{
        allocator::Fourcc,
        input::{ButtonState, InputTime, KeyState},
        renderer::{
            Bind, ExportMem, Offscreen, damage::OutputDamageTracker, pixman::PixmanRenderer,
        },
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{EventLoop, channel},
        pixman::Image,
        wayland_server::Display,
    },
    utils::{Buffer, Rectangle, Size, Transform},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_pointer::{self, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::WlSeat,
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup::{self, XdgPopup},
    xdg_positioner::{self, XdgPositioner},
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};
use wirbel::{Wirbel, config::Config, input::Scroll, layout::Rect, workspace::Placement};

const WIDTH: i32 = 1000;
const HEIGHT: i32 = 600;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

// Evdev key codes.
const KEY_1: u32 = 2;
const KEY_2: u32 = 3;
const KEY_3: u32 = 4;
const KEY_Q: u32 = 16;
const KEY_W: u32 = 17;
const KEY_Y: u32 = 21; // z on a German keyboard
const KEY_R: u32 = 19;
const KEY_F: u32 = 33;
const KEY_A: u32 = 30;
const KEY_ENTER: u32 = 28;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTALT: u32 = 56;
const KEY_LEFTMETA: u32 = 125;
const KEY_UP: u32 = 103;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_DOWN: u32 = 108;
const KEY_TAB: u32 = 15;

type Job = Box<dyn FnOnce(&mut Wirbel) + Send>;

struct Compositor {
    jobs: channel::Sender<Job>,
    socket: PathBuf,
    thread: Option<JoinHandle<()>>,
}

impl Compositor {
    /// The default configuration with focus on click, which is what most tests want to see.
    fn start() -> Self {
        Self::start_with("sloppy_focus = 0\nwarp_cursor = 0\n")
    }

    fn start_with(config: &str) -> Self {
        let (config, errors) = Config::parse(config);
        assert_eq!(errors, []);
        let (jobs, job_source) = channel::channel::<Job>();
        let (socket_tx, socket_rx) = mpsc::channel();

        let thread = thread::spawn(move || {
            let mut event_loop = EventLoop::try_new().unwrap();
            let display = Display::new().unwrap();
            let mut state = Wirbel::new(&mut event_loop, display, config).unwrap();

            let output = test_output("test", (0, 0));
            let _global = output.create_global::<Wirbel>(&state.display_handle);
            state.add_output(&output);

            event_loop
                .handle()
                .insert_source(job_source, |event, _, state| {
                    if let channel::Event::Msg(job) = event {
                        job(state);
                    }
                })
                .unwrap();

            socket_tx.send(state.socket_name.clone()).unwrap();
            event_loop
                .run(None, &mut state, Wirbel::after_dispatch)
                .unwrap();
        });

        let name = socket_rx.recv().unwrap();
        let socket = PathBuf::from(env::var_os("XDG_RUNTIME_DIR").unwrap()).join(name);
        Self {
            jobs,
            socket,
            thread: Some(thread),
        }
    }

    /// Runs `job` on the compositor thread and returns its result.
    fn with<R: Send + 'static>(&self, job: impl FnOnce(&mut Wirbel) -> R + Send + 'static) -> R {
        let (tx, rx) = mpsc::channel();
        self.jobs
            .send(Box::new(move |state| tx.send(job(state)).unwrap()))
            .unwrap();
        rx.recv().unwrap()
    }

    fn pointer_at(&self, x: f64, y: f64) {
        self.with(move |state| state.on_pointer_motion((x, y).into(), InputTime::now()));
    }

    fn click(&self) {
        self.with(|state| {
            state.on_pointer_button(BTN_LEFT, ButtonState::Pressed, InputTime::now());
            state.on_pointer_button(BTN_LEFT, ButtonState::Released, InputTime::now());
        });
    }

    fn button(&self, button: u32, button_state: ButtonState) {
        self.with(move |state| state.on_pointer_button(button, button_state, InputTime::now()));
    }

    fn key(&self, evdev: u32, key_state: KeyState) {
        self.with(move |state| {
            state.on_key((evdev + 8).into(), key_state, InputTime::now());
        });
    }

    fn tap_key(&self, evdev: u32) {
        self.key(evdev, KeyState::Pressed);
        self.key(evdev, KeyState::Released);
    }

    /// Taps `key` with `modifiers` held.
    fn chord(&self, modifiers: &[u32], key: u32) {
        for modifier in modifiers {
            self.key(*modifier, KeyState::Pressed);
        }
        self.tap_key(key);
        for modifier in modifiers.iter().rev() {
            self.key(*modifier, KeyState::Released);
        }
    }

    /// Adds a 1000x600 output at `location` and returns it.
    fn add_output(&self, name: &'static str, location: (i32, i32)) -> Output {
        self.with(move |state| {
            let output = test_output(name, location);
            let _global = output.create_global::<Wirbel>(&state.display_handle);
            state.add_output(&output);
            output
        })
    }

    fn focused_rect(&self) -> Option<Rect> {
        self.with(|state| {
            let focused = state.focused.as_ref()?;
            let frames = state.frames.iter();
            frames
                .into_iter()
                .find(|frame| frame.window == *focused)
                .map(|frame| frame.inner)
        })
    }

    /// Geometry of all visible windows, by rectangle of their content.
    fn frames(&self) -> Vec<Rect> {
        self.with(|state| state.frames.iter().map(|frame| frame.inner).collect())
    }

    fn active_workspace(&self) -> usize {
        self.with(|state| state.monitors[state.active_monitor].active)
    }

    fn screenshot(&self) -> Screenshot {
        self.screenshot_of(0)
    }

    /// Renders an output with pixman into XRGB pixels.
    fn screenshot_of(&self, monitor: usize) -> Screenshot {
        let pixels = self.with(move |state| {
            // Runs once per loop iteration normally. Output tracking of new windows needs it.
            state.space.refresh();
            let output = state.monitors[monitor].output.clone();
            let background = state.config.background_color;
            let size = Size::<i32, Buffer>::from((WIDTH, HEIGHT));

            let mut renderer = PixmanRenderer::new().unwrap();
            let mut image: Image<'static, 'static> =
                renderer.create_buffer(Fourcc::Argb8888, size).unwrap();
            let mut framebuffer = renderer.bind(&mut image).unwrap();

            let elements = state.scene_elements(&mut renderer, &output);
            OutputDamageTracker::from_output(&output)
                .render_output(&mut renderer, &mut framebuffer, 0, &elements, background)
                .unwrap();

            let mapping = renderer
                .copy_framebuffer(&framebuffer, Rectangle::from_size(size), Fourcc::Argb8888)
                .unwrap();
            renderer.map_texture(&mapping).unwrap().to_vec()
        });
        Screenshot { pixels }
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        let stop: Job = Box::new(|state| state.loop_signal.stop());
        if self.jobs.send(stop).is_ok()
            && let Some(thread) = self.thread.take()
        {
            let result = thread.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

fn test_output(name: &str, location: (i32, i32)) -> Output {
    let output = Output::new(
        name.to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "wirbel".into(),
            model: "headless".into(),
            serial_number: "0".into(),
        },
    );
    let mode = Mode {
        size: (WIDTH, HEIGHT).into(),
        refresh: 60_000,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some(location.into()),
    );
    output.set_preferred(mode);
    output
}

struct Screenshot {
    pixels: Vec<u8>,
}

impl Screenshot {
    /// The pixel as 0xRRGGBB.
    fn at(&self, x: i32, y: i32) -> u32 {
        let offset = ((y * WIDTH + x) * 4) as usize;
        let bytes = &self.pixels[offset..offset + 4];
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
    }
}

fn rgb(color: [f32; 4]) -> u32 {
    let channel = |value: f32| (value * 255.0).round() as u32;
    channel(color[0]) << 16 | channel(color[1]) << 8 | channel(color[2])
}

#[derive(Debug, PartialEq)]
enum Input {
    KeyboardEnter(usize),
    KeyboardLeave(usize),
    Key(u32, wl_keyboard::KeyState),
    PointerEnter(usize, f64, f64),
    PointerLeave(usize),
    PointerMotion(f64, f64),
    Button(u32, wl_pointer::ButtonState),
    AxisSource(wl_pointer::AxisSource),
    Axis(wl_pointer::Axis, f64),
    AxisV120(wl_pointer::Axis, i32),
    AxisStop(wl_pointer::Axis),
}

#[derive(Default)]
struct ClientState {
    windows: Vec<WindowState>,
    input: Vec<Input>,
    /// Position of the latest popup configure, relative to the parent window.
    popup_position: Option<(i32, i32)>,
    /// Serial of the latest button press, for interactive move and resize.
    button_serial: u32,
    closed: Vec<usize>,
    /// Rate and delay of the keyboard.
    repeat: Option<(i32, i32)>,
}

struct WindowState {
    surface: WlSurface,
    xdg_surface: XdgSurface,
    toplevel: XdgToplevel,
    /// Size of the latest configure.
    size: (i32, i32),
    activated: bool,
    states: Vec<xdg_toplevel::State>,
    configures: u32,
    buffer: Option<WlBuffer>,
}

impl ClientState {
    fn window_index(&self, surface: &WlSurface) -> usize {
        self.find_window(surface).unwrap()
    }

    /// `None` if the client already destroyed the surface. A leave for such a surface crosses
    /// paths with the destroy request and is not an error.
    fn find_window(&self, surface: &WlSurface) -> Option<usize> {
        self.windows
            .iter()
            .position(|window| window.surface == *surface)
    }
}

struct Client {
    connection: Connection,
    queue: EventQueue<ClientState>,
    state: ClientState,
    handle: QueueHandle<ClientState>,
    globals: GlobalList,
    compositor: WlCompositor,
    wm_base: XdgWmBase,
    shm: WlShm,
    _keyboard: WlKeyboard,
    _pointer: WlPointer,
}

impl Client {
    fn connect(compositor: &Compositor) -> Self {
        let stream = UnixStream::connect(&compositor.socket).unwrap();
        let connection = Connection::from_socket(stream).unwrap();
        let (globals, queue) = registry_queue_init::<ClientState>(&connection).unwrap();
        let handle = queue.handle();

        let wl_compositor: WlCompositor = globals.bind(&handle, 1..=6, ()).unwrap();
        let wm_base: XdgWmBase = globals.bind(&handle, 1..=6, ()).unwrap();
        let shm: WlShm = globals.bind(&handle, 1..=1, ()).unwrap();
        let seat: WlSeat = globals.bind(&handle, 1..=9, ()).unwrap();

        let mut client = Self {
            connection,
            state: ClientState::default(),
            globals,
            compositor: wl_compositor,
            wm_base,
            shm,
            _keyboard: seat.get_keyboard(&handle, ()),
            _pointer: seat.get_pointer(&handle, ()),
            queue,
            handle,
        };
        client.sync();
        client
    }

    fn sync(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }

    /// Opens a toplevel and walks it through configure up to the first buffer.
    fn open_window(&mut self, color: u32) -> usize {
        let index = self.open_unmapped_window();
        self.draw(index, color);
        index
    }

    /// Opens a toplevel and stops after the initial configure.
    fn open_unmapped_window(&mut self) -> usize {
        self.open_unmapped_window_with(|_| {})
    }

    /// Like `open_unmapped_window`, with a chance to set the window up before the first commit.
    fn open_unmapped_window_with(&mut self, setup: impl FnOnce(&XdgToplevel)) -> usize {
        let surface = self.compositor.create_surface(&self.handle, ());
        let xdg_surface = self.wm_base.get_xdg_surface(&surface, &self.handle, ());
        let toplevel = xdg_surface.get_toplevel(&self.handle, ());
        setup(&toplevel);
        surface.commit();
        self.state.windows.push(WindowState {
            surface,
            xdg_surface,
            toplevel,
            size: (0, 0),
            activated: false,
            states: Vec::new(),
            configures: 0,
            buffer: None,
        });
        self.sync();
        let index = self.state.windows.len() - 1;
        assert!(
            self.state.windows[index].configures > 0,
            "no initial configure"
        );
        index
    }

    /// Opens a window that picks its own size, as dialogs do.
    fn open_sized_window(
        &mut self,
        size: (i32, i32),
        color: u32,
        setup: impl FnOnce(&XdgToplevel),
    ) -> usize {
        let index = self.open_unmapped_window_with(setup);
        assert_eq!(self.size(index), (0, 0), "dialog was told a size");
        self.draw_sized(index, size, color);
        index
    }

    /// Fills the window with `color`, using the size of the last configure.
    fn draw(&mut self, index: usize, color: u32) {
        self.draw_sized(index, self.state.windows[index].size, color);
    }

    fn draw_sized(&mut self, index: usize, (width, height): (i32, i32), color: u32) {
        let surface = self.state.windows[index].surface.clone();
        let buffer = self.fill(&surface, width, height, color);
        self.state.windows[index].buffer = Some(buffer);
    }

    fn fill(&mut self, surface: &WlSurface, width: i32, height: i32, color: u32) -> WlBuffer {
        static NEXT_FILE: AtomicU32 = AtomicU32::new(0);

        let path = env::temp_dir().join(format!(
            "wirbel-test-{}-{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        fs::remove_file(&path).unwrap();
        let pixels: Vec<u8> = color.to_le_bytes().repeat((width * height) as usize);
        file.write_all(&pixels).unwrap();

        let pool = self
            .shm
            .create_pool(file.as_fd(), pixels.len() as i32, &self.handle, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            Format::Xrgb8888,
            &self.handle,
            (),
        );
        pool.destroy();

        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, width, height);
        surface.commit();
        self.sync();
        buffer
    }

    /// Opens a 100x50 popup anchored near the right edge of a window filling its output, which
    /// would stick out of the output.
    fn open_edge_popup(&mut self, index: usize) -> (WlSurface, XdgPopup) {
        let positioner = self.wm_base.create_positioner(&self.handle, ());
        positioner.set_size(100, 50);
        positioner.set_anchor_rect(960, 20, 1, 1);
        positioner.set_anchor(xdg_positioner::Anchor::BottomRight);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::SlideX);

        let surface = self.compositor.create_surface(&self.handle, ());
        let xdg_surface = self.wm_base.get_xdg_surface(&surface, &self.handle, ());
        let parent = self.state.windows[index].xdg_surface.clone();
        let popup = xdg_surface.get_popup(Some(&parent), &positioner, &self.handle, ());
        surface.commit();
        self.sync();
        (surface, popup)
    }

    fn close(&mut self, index: usize) {
        let window = &self.state.windows[index];
        window.toplevel.destroy();
        window.xdg_surface.destroy();
        window.surface.destroy();
        self.sync();
    }

    fn take_input(&mut self) -> Vec<Input> {
        self.sync();
        std::mem::take(&mut self.state.input)
    }

    fn size(&self, index: usize) -> (i32, i32) {
        self.state.windows[index].size
    }

    fn activated(&self, index: usize) -> bool {
        self.state.windows[index].activated
    }

    fn has_state(&self, index: usize, state: xdg_toplevel::State) -> bool {
        self.state.windows[index].states.contains(&state)
    }

    fn toplevel(&self, index: usize) -> XdgToplevel {
        self.state.windows[index].toplevel.clone()
    }

    fn seat(&self) -> WlSeat {
        self.globals.bind(&self.handle, 1..=9, ()).unwrap()
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for ClientState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmBase, ()> for ClientState {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for ClientState {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            if let Some(window) = state
                .windows
                .iter_mut()
                .find(|w| w.xdg_surface == *xdg_surface)
            {
                window.configures += 1;
            }
        }
    }
}

impl Dispatch<XdgToplevel, ()> for ClientState {
    fn event(
        state: &mut Self,
        toplevel: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let index = state.windows.iter().position(|w| w.toplevel == *toplevel);
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                let Some(index) = index else { return };
                let window = &mut state.windows[index];
                window.size = (width, height);
                window.states = states
                    .chunks_exact(4)
                    .filter_map(|chunk| {
                        let value = u32::from_ne_bytes(chunk.try_into().unwrap());
                        xdg_toplevel::State::try_from(value).ok()
                    })
                    .collect();
                window.activated = window.states.contains(&xdg_toplevel::State::Activated);
            }
            xdg_toplevel::Event::Close => state.closed.extend(index),
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let input = match event {
            wl_keyboard::Event::Enter { surface, .. } => {
                Input::KeyboardEnter(state.window_index(&surface))
            }
            wl_keyboard::Event::Leave { surface, .. } => match state.find_window(&surface) {
                Some(index) => Input::KeyboardLeave(index),
                None => return,
            },
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } => Input::Key(key, key_state),
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.repeat = Some((rate, delay));
                return;
            }
            _ => return,
        };
        state.input.push(input);
    }
}

impl Dispatch<WlPointer, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let input = match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => Input::PointerEnter(state.window_index(&surface), surface_x, surface_y),
            wl_pointer::Event::Leave { surface, .. } => match state.find_window(&surface) {
                Some(index) => Input::PointerLeave(index),
                None => return,
            },
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => Input::PointerMotion(surface_x, surface_y),
            wl_pointer::Event::Button {
                button,
                serial,
                state: WEnum::Value(button_state),
                ..
            } => {
                state.button_serial = serial;
                Input::Button(button, button_state)
            }
            wl_pointer::Event::AxisSource {
                axis_source: WEnum::Value(source),
            } => Input::AxisSource(source),
            wl_pointer::Event::Axis {
                axis: WEnum::Value(axis),
                value,
                ..
            } => Input::Axis(axis, value),
            wl_pointer::Event::AxisValue120 {
                axis: WEnum::Value(axis),
                value120,
            } => Input::AxisV120(axis, value120),
            wl_pointer::Event::AxisStop {
                axis: WEnum::Value(axis),
                ..
            } => Input::AxisStop(axis),
            _ => return,
        };
        state.input.push(input);
    }
}

impl Dispatch<XdgPopup, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &XdgPopup,
        event: xdg_popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_popup::Event::Configure { x, y, .. } = event {
            state.popup_position = Some((x, y));
        }
    }
}

delegate_noop!(ClientState: ignore XdgPositioner);
delegate_noop!(ClientState: ignore WlCompositor);
delegate_noop!(ClientState: ignore WlSurface);
delegate_noop!(ClientState: ignore WlShm);
delegate_noop!(ClientState: ignore WlShmPool);
delegate_noop!(ClientState: ignore WlBuffer);
delegate_noop!(ClientState: ignore WlSeat);

// Output 1000x600, outer gap 10, inner gap 5, border 4. One window fills the output minus gap and
// border. Two windows share 975 pixels of width: tiles of 488 and 487 pixels.
const FULL: (i32, i32) = (972, 572);
const LEFT: (i32, i32) = (480, 572);
const RIGHT: (i32, i32) = (479, 572);
const RIGHT_X: f64 = 507.0;

#[test]
fn offers_the_globals_kitty_needs() {
    let compositor = Compositor::start();
    let client = Client::connect(&compositor);
    let names: Vec<String> = client
        .globals
        .contents()
        .clone_list()
        .into_iter()
        .map(|global| global.interface)
        .collect();
    for name in [
        "wl_compositor",
        "wl_shm",
        "wl_seat",
        "wl_output",
        "xdg_wm_base",
    ] {
        assert!(
            names.iter().any(|global| global == name),
            "{name} missing in {names:?}"
        );
    }
    client.connection.flush().unwrap();
}

#[test]
fn windows_are_tiled_and_focused_as_they_open_and_close() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);

    let a = client.open_window(0x00ff_0000);
    assert_eq!(client.size(a), FULL);
    assert!(client.activated(a));
    assert_eq!(client.take_input(), [Input::KeyboardEnter(a)]);

    let b = client.open_window(0x0000_ff00);
    assert_eq!(client.size(a), LEFT);
    assert_eq!(client.size(b), RIGHT);
    assert!(!client.activated(a));
    assert!(client.activated(b));
    assert_eq!(
        client.take_input(),
        [Input::KeyboardLeave(a), Input::KeyboardEnter(b)]
    );

    client.close(b);
    assert_eq!(client.size(a), FULL);
    assert!(client.activated(a));
    assert_eq!(client.take_input(), [Input::KeyboardEnter(a)]);
    assert!(compositor.with(|state| state.monitors[0].workspace().tiled.items().len() == 1));
}

#[test]
fn window_gets_no_focus_before_its_first_buffer() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_unmapped_window();
    assert_eq!(client.size(a), FULL);
    assert_eq!(client.take_input(), []);
    client.draw(a, 0x00ff_0000);
    assert_eq!(client.take_input(), [Input::KeyboardEnter(a)]);
}

#[test]
fn keys_reach_only_the_focused_window() {
    let compositor = Compositor::start();
    let mut other = Client::connect(&compositor);
    let mut client = Client::connect(&compositor);
    let _ = other.open_window(0x00ff_0000);
    let b = client.open_window(0x0000_ff00);
    other.take_input();
    client.take_input();

    const KEY_A: u32 = 30;
    compositor.tap_key(KEY_A);
    assert_eq!(
        client.take_input(),
        [
            Input::Key(KEY_A, wl_keyboard::KeyState::Pressed),
            Input::Key(KEY_A, wl_keyboard::KeyState::Released),
        ]
    );
    assert_eq!(other.take_input(), []);
    assert!(client.activated(b));
}

#[test]
fn pointer_enters_with_surface_local_coordinates() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    let b = client.open_window(0x0000_ff00);
    client.draw(a, 0x00ff_0000);
    client.take_input();

    compositor.pointer_at(114.0, 44.5);
    assert_eq!(client.take_input(), [Input::PointerEnter(a, 100.0, 30.5)]);

    compositor.pointer_at(115.0, 44.5);
    assert_eq!(client.take_input(), [Input::PointerMotion(101.0, 30.5)]);

    compositor.pointer_at(RIGHT_X + 3.0, 50.0);
    assert_eq!(
        client.take_input(),
        [Input::PointerLeave(a), Input::PointerEnter(b, 3.0, 36.0)]
    );

    // The gap between the windows belongs to nobody.
    compositor.pointer_at(500.0, 50.0);
    assert_eq!(client.take_input(), [Input::PointerLeave(b)]);
}

#[test]
fn click_moves_focus_and_reaches_the_client() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    let b = client.open_window(0x0000_ff00);
    compositor.pointer_at(100.0, 100.0);
    client.take_input();
    assert!(client.activated(b));

    compositor.click();
    assert_eq!(
        client.take_input(),
        [
            Input::KeyboardLeave(b),
            Input::KeyboardEnter(a),
            Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed),
            Input::Button(BTN_LEFT, wl_pointer::ButtonState::Released),
        ]
    );
    assert!(client.activated(a));
    assert!(!client.activated(b));
}

#[test]
fn clicking_the_background_keeps_focus() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    compositor.pointer_at(2.0, 2.0);
    compositor.click();
    assert!(client.activated(a));
    assert_eq!(client.take_input(), [Input::KeyboardEnter(a)]);
}

#[test]
fn pointer_focus_follows_windows_that_move_under_it() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    compositor.pointer_at(800.0, 300.0);
    assert_eq!(
        client.take_input(),
        [
            Input::KeyboardEnter(a),
            Input::PointerEnter(a, 786.0, 286.0)
        ]
    );

    // The new window takes the right half, where the pointer rests.
    let b = client.open_window(0x0000_ff00);
    let input = client.take_input();
    assert!(input.contains(&Input::PointerLeave(a)), "{input:?}");
    assert!(
        input.contains(&Input::PointerEnter(b, 800.0 - RIGHT_X, 286.0)),
        "{input:?}"
    );
}

#[test]
fn pointer_enters_a_window_that_grows_under_it() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    let b = client.open_window(0x0000_ff00);
    client.draw(a, 0x00ff_0000);
    compositor.pointer_at(800.0, 300.0);
    client.take_input();

    client.close(b);
    assert_eq!(client.take_input(), [Input::KeyboardEnter(a)]);

    client.draw(a, 0x00ff_0000);
    assert_eq!(client.take_input(), [Input::PointerEnter(a, 786.0, 286.0)]);

    // Further commits from the same window are not pointer events.
    client.draw(a, 0x00ff_0000);
    assert_eq!(client.take_input(), []);
}

#[test]
fn output_resize_retiles() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let mode = Mode {
            size: (600, 400).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        state.retile();
    });
    client.sync();
    assert_eq!(client.size(a), (572, 372));
}

#[test]
fn null_buffer_unmaps_and_the_window_can_return() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    let b = client.open_window(0x0000_ff00);
    client.take_input();

    let surface = client.state.windows[b].surface.clone();
    surface.attach(None, 0, 0);
    surface.commit();
    client.sync();
    assert_eq!(client.size(a), FULL);
    assert_eq!(
        client.take_input(),
        [Input::KeyboardLeave(b), Input::KeyboardEnter(a)]
    );
    assert!(compositor.with(|state| state.monitors[0].workspace().tiled.items().len() == 1));

    surface.commit();
    client.sync();
    assert_eq!(client.size(a), LEFT);
    assert_eq!(client.size(b), RIGHT);
    client.draw(b, 0x0000_ff00);
    assert_eq!(
        client.take_input(),
        [Input::KeyboardLeave(a), Input::KeyboardEnter(b)]
    );
}

#[test]
fn disconnecting_removes_all_windows_of_the_client() {
    let compositor = Compositor::start();
    let mut survivor = Client::connect(&compositor);
    let a = survivor.open_window(0x00ff_0000);
    {
        let mut client = Client::connect(&compositor);
        client.open_window(0x0000_ff00);
        client.open_window(0x0000_00ff);
        survivor.sync();
        assert!(compositor.with(|state| state.monitors[0].workspace().tiled.items().len() == 3));
    }
    survivor.sync();
    // The server sees the hangup asynchronously.
    for _ in 0..100 {
        if compositor.with(|state| state.monitors[0].workspace().tiled.items().len() == 1) {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
        survivor.sync();
    }
    assert_eq!(survivor.size(a), FULL);
    assert!(survivor.activated(a));
}

#[test]
fn popups_stay_on_the_output_and_are_drawn_above_their_window() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);

    let (surface, _popup) = client.open_edge_popup(a);

    // The window starts at x=14, so x=886 puts the right edge exactly on the 1000 wide output.
    assert_eq!(client.state.popup_position, Some((886, 21)));

    let _buffer = client.fill(&surface, 100, 50, 0x0000_00ff);
    let shot = compositor.screenshot();
    assert_eq!(shot.at(14 + 886, 14 + 21), 0x00_00ff);
    assert_eq!(shot.at(999, 14 + 70), 0x00_00ff);
    assert_eq!(shot.at(14 + 885, 14 + 21), 0xff_0000);
}

#[test]
fn renders_windows_borders_and_background() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(0x00ff_0000);
    client.open_window(0x0000_ff00);
    // The first window was resized by the second one and has to catch up.
    client.draw(0, 0x00ff_0000);

    let shot = compositor.screenshot();
    let background = rgb(Config::default().background_color);
    let border = rgb(Config::default().border_color);
    let focus = rgb(Config::default().focus_color);

    assert_eq!(shot.at(0, 0), background, "outer gap");
    assert_eq!(shot.at(10, 10), border, "corner of the first tile");
    assert_eq!(shot.at(13, 300), border, "left border");
    assert_eq!(shot.at(14, 300), 0xff_0000, "first window");
    assert_eq!(shot.at(493, 300), 0xff_0000, "first window, last column");
    assert_eq!(shot.at(494, 300), border, "right border of the first tile");
    assert_eq!(shot.at(500, 300), background, "inner gap");
    assert_eq!(shot.at(503, 300), focus, "border of the focused tile");
    assert_eq!(shot.at(507, 300), 0x00_ff00, "second window");
    assert_eq!(shot.at(985, 300), 0x00_ff00, "second window, last column");
    assert_eq!(shot.at(986, 300), focus, "right border");
    assert_eq!(shot.at(989, 589), focus, "corner of the second tile");
    assert_eq!(shot.at(990, 590), background, "outer gap at the far corner");
}

#[test]
fn border_follows_focus() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(0x00ff_0000);
    client.open_window(0x0000_ff00);
    client.draw(a, 0x00ff_0000);
    compositor.pointer_at(100.0, 100.0);
    compositor.click();
    client.sync();

    let shot = compositor.screenshot();
    assert_eq!(shot.at(10, 300), rgb(Config::default().focus_color));
    assert_eq!(shot.at(503, 300), rgb(Config::default().border_color));
}

// Stage 2: binds, workspaces, floating, fullscreen, mouse.

const SUPER: &[u32] = &[KEY_LEFTMETA];
const SUPER_SHIFT: &[u32] = &[KEY_LEFTMETA, KEY_LEFTSHIFT];
const ALT: &[u32] = &[KEY_LEFTALT];
const RED: u32 = 0x00ff_0000;
const GREEN: u32 = 0x0000_ff00;
const BLUE: u32 = 0x0000_00ff;

/// A compositor with windows `a` and `b` side by side, `b` focused.
fn two_windows() -> (Compositor, Client, usize, usize) {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    client.take_input();
    (compositor, client, a, b)
}

/// A scratch file path for one test.
fn scratch_path(name: &str) -> PathBuf {
    env::temp_dir().join(format!("wirbel-test-{}-{name}", std::process::id()))
}

fn is_tiled(compositor: &Compositor) -> Vec<bool> {
    compositor.with(|state| state.frames.iter().map(|frame| frame.tiled).collect())
}

#[test]
fn a_bound_key_never_reaches_the_client() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    client.take_input();
    compositor.tap_key(KEY_2);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    // The window lost focus through the bind, but no key event got out.
    assert!(
        !client
            .take_input()
            .iter()
            .any(|input| matches!(input, Input::Key(code, _) if *code == KEY_2)),
    );
    assert_eq!(compositor.active_workspace(), 1);
}

#[test]
fn an_unbound_combination_reaches_the_client() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.chord(SUPER, KEY_Q);
    assert_eq!(
        client.take_input(),
        [
            Input::Key(KEY_LEFTMETA, wl_keyboard::KeyState::Pressed),
            Input::Key(KEY_Q, wl_keyboard::KeyState::Pressed),
            Input::Key(KEY_Q, wl_keyboard::KeyState::Released),
            Input::Key(KEY_LEFTMETA, wl_keyboard::KeyState::Released),
        ]
    );
}

#[test]
fn a_key_released_after_its_modifier_stays_swallowed() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.key(KEY_LEFTALT, KeyState::Pressed);
    compositor.key(KEY_LEFT, KeyState::Pressed);
    compositor.key(KEY_LEFTALT, KeyState::Released);
    compositor.key(KEY_LEFT, KeyState::Released);
    assert!(
        !client
            .take_input()
            .iter()
            .any(|input| matches!(input, Input::Key(code, _) if *code == KEY_LEFT))
    );
    // And the same key is an ordinary key again afterwards.
    compositor.tap_key(KEY_LEFT);
    assert!(
        client
            .take_input()
            .contains(&Input::Key(KEY_LEFT, wl_keyboard::KeyState::Pressed))
    );
}

#[test]
fn spawn_runs_the_command_of_the_bind() {
    let marker = scratch_path("spawned");
    let compositor = Compositor::start_with(&format!(
        "bind = SUPER, Return, spawn, touch {}\n",
        marker.display()
    ));
    compositor.chord(SUPER, KEY_ENTER);
    for _ in 0..200 {
        if marker.exists() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(marker.exists());
    fs::remove_file(marker).unwrap();
}

#[test]
fn close_asks_the_focused_window_only() {
    let (compositor, mut client, _, b) = two_windows();
    compositor.chord(SUPER, KEY_W);
    client.sync();
    assert_eq!(client.state.closed, [b]);
}

#[test]
fn quit_ends_the_event_loop() {
    let mut compositor = Compositor::start();
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    // The job that runs this is the last thing the loop does.
    let keycode = 50u32 + 8; // M
    let job: Job = Box::new(move |state| {
        state.on_key(keycode.into(), KeyState::Pressed, InputTime::now());
    });
    compositor.jobs.send(job).unwrap();
    compositor.thread.take().unwrap().join().unwrap();
}

#[test]
fn the_default_keymap_is_german() {
    let compositor = Compositor::start_with("bind = SUPER, z, focus_next\n");
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_window(GREEN);
    client.take_input();
    let before = compositor.focused_rect();
    compositor.chord(SUPER, KEY_Y);
    assert_ne!(compositor.focused_rect(), before);
}

#[test]
fn the_keymap_comes_from_the_config() {
    let compositor = Compositor::start_with("keyboard_layout = us\nbind = SUPER, y, focus_next\n");
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_window(GREEN);
    let before = compositor.focused_rect();
    compositor.chord(SUPER, KEY_Y);
    assert_ne!(compositor.focused_rect(), before);
}

#[test]
fn a_shifted_digit_matches_by_its_unshifted_symbol() {
    let (compositor, _client, _, _) = two_windows();
    // On a German keyboard Shift+2 produces a quotation mark.
    compositor.chord(SUPER_SHIFT, KEY_2);
    assert_eq!(compositor.active_workspace(), 1);
    assert_eq!(compositor.frames().len(), 1);
}

#[test]
fn the_keyboard_repeat_comes_from_the_config() {
    let compositor = Compositor::start_with("repeat_delay = 123\nrepeat_rate = 45\n");
    let client = Client::connect(&compositor);
    assert_eq!(client.state.repeat, Some((45, 123)));
}

#[test]
fn reload_applies_a_changed_file_to_the_running_compositor() {
    let path = scratch_path("reload");
    fs::write(&path, "gap_outer = 0\n").unwrap();
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    assert_eq!(client.size(a), FULL);

    let config_path = path.clone();
    compositor.with(move |state| state.config_path = Some(config_path));
    compositor.chord(SUPER, KEY_R);
    client.sync();
    assert_eq!(client.size(a), (992, 592));

    // Binds and keymap are live as well.
    fs::write(
        &path,
        "keyboard_layout = us\nrepeat_rate = 7\nbind = SUPER, y, float\n",
    )
    .unwrap();
    compositor.chord(SUPER, KEY_R);
    assert_eq!(compositor.with(|state| state.config.binds.len()), 2);
    client.sync();
    assert_eq!(client.state.repeat, Some((7, 600)));
    compositor.chord(SUPER, KEY_Y);
    assert_eq!(is_tiled(&compositor), [false]);
    fs::remove_file(path).unwrap();
}

#[test]
fn reload_keeps_the_running_config_if_the_new_one_is_broken() {
    let path = scratch_path("reload-broken");
    let compositor = Compositor::start();
    let config_path = path.clone();
    compositor.with(move |state| state.config_path = Some(config_path));
    let before = compositor.with(|state| state.config.clone());

    for text in [
        "gap_outer = 3\nwobble\n",
        "gap_outer = 3\nkeyboard_layout = nosuchlayout\n",
    ] {
        fs::write(&path, text).unwrap();
        compositor.chord(SUPER, KEY_R);
        let config = compositor.with(|state| state.config.clone());
        assert_eq!(config, before, "{text}");
    }

    // The keymap in use survives the failed attempt.
    assert_eq!(
        compositor.with(|state| {
            let keyboard = state.seat.get_keyboard().unwrap();
            keyboard.with_xkb_state(state, |context| {
                let xkb = context.xkb().lock().unwrap();
                xkb.layout_name(xkb.active_layout()).to_string()
            })
        }),
        "German"
    );
    fs::remove_file(path).unwrap();

    // A deleted file means the defaults, as it does at startup.
    compositor.chord(SUPER, KEY_R);
    assert_eq!(
        compositor.with(|state| state.config.clone()),
        Config::default()
    );
}

#[test]
fn reload_without_a_path_does_nothing() {
    let compositor = Compositor::start();
    let before = compositor.with(|state| state.config.clone());
    compositor.chord(SUPER, KEY_R);
    assert_eq!(compositor.with(|state| state.config.clone()), before);
}

#[test]
fn focus_moves_by_direction_and_stops_at_the_edge() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    // a on the left, b top right, c bottom right.
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    let c = client.open_window(BLUE);
    client.sync();
    assert!(client.activated(c));

    compositor.chord(ALT, KEY_UP);
    client.sync();
    assert!(client.activated(b));
    compositor.chord(ALT, KEY_LEFT);
    client.sync();
    assert!(client.activated(a));
    compositor.chord(ALT, KEY_LEFT);
    client.sync();
    assert!(client.activated(a));
    compositor.chord(ALT, KEY_RIGHT);
    client.sync();
    assert!(client.activated(b));
    compositor.chord(ALT, KEY_DOWN);
    client.sync();
    assert!(client.activated(c));
}

#[test]
fn focus_next_cycles_through_the_workspace() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(SUPER, KEY_TAB);
    client.sync();
    assert!(client.activated(a));
    compositor.chord(SUPER, KEY_TAB);
    client.sync();
    assert!(client.activated(b));
}

#[test]
fn swap_exchanges_tiles_and_keeps_focus_on_the_window() {
    let (compositor, mut client, a, b) = two_windows();
    let frames = compositor.frames();
    compositor.chord(SUPER_SHIFT, KEY_LEFT);
    client.sync();
    assert_eq!(client.size(b), LEFT);
    assert_eq!(client.size(a), RIGHT);
    assert!(client.activated(b));
    assert_eq!(compositor.focused_rect(), Some(frames[0]));

    // Nothing further left to swap with.
    compositor.chord(SUPER_SHIFT, KEY_LEFT);
    client.sync();
    assert_eq!(client.size(b), LEFT);
}

#[test]
fn split_ratio_grows_the_focused_tile_from_either_side() {
    let (compositor, mut client, a, b) = two_windows();
    let right = &[KEY_LEFTCTRL, KEY_LEFTALT];
    compositor.chord(right, KEY_RIGHT);
    client.sync();
    assert!(client.size(b).0 > RIGHT.0, "{:?}", client.size(b));
    assert!(client.size(a).0 < LEFT.0);

    compositor.chord(right, KEY_LEFT);
    compositor.chord(right, KEY_LEFT);
    client.sync();
    assert!(client.size(b).0 < RIGHT.0);
    assert!(client.size(a).0 > LEFT.0);
}

#[test]
fn split_ratio_ignores_a_single_window() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.chord(&[KEY_LEFTCTRL, KEY_LEFTALT], KEY_RIGHT);
    client.sync();
    assert_eq!(client.size(a), FULL);
}

#[test]
fn workspaces_hide_and_restore_their_windows() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(SUPER, KEY_2);
    client.sync();
    assert!(compositor.frames().is_empty());
    assert_eq!(compositor.with(|state| state.space.elements().count()), 0);
    assert_eq!(compositor.focused_rect(), None);
    assert!(client.take_input().contains(&Input::KeyboardLeave(b)));

    // Workspace 2 tiles on its own.
    let c = client.open_window(BLUE);
    assert_eq!(client.size(c), FULL);
    assert_eq!(client.size(a), LEFT);
    let shot = compositor.screenshot();
    assert_eq!(shot.at(250, 300), BLUE & 0xff_ffff);

    compositor.chord(SUPER, KEY_1);
    client.sync();
    assert!(client.activated(b), "focus did not come back");
    assert!(!client.activated(c));
    assert_eq!(compositor.frames().len(), 2);
    let shot = compositor.screenshot();
    assert_eq!(shot.at(250, 300), RED & 0xff_ffff);
    assert_eq!(shot.at(750, 300), GREEN & 0xff_ffff);
}

#[test]
fn input_does_not_reach_a_hidden_window() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.pointer_at(250.0, 300.0);
    compositor.chord(SUPER, KEY_2);
    client.take_input();
    compositor.pointer_at(260.0, 310.0);
    compositor.click();
    compositor.tap_key(KEY_Q);
    assert_eq!(client.take_input(), []);
}

#[test]
fn a_window_on_a_hidden_workspace_keeps_living() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(SUPER, KEY_2);
    client.sync();

    // It may close there, and its workspace rearranges for the next visit.
    client.close(b);
    compositor.chord(SUPER, KEY_1);
    client.sync();
    assert_eq!(client.size(a), FULL);
    assert!(client.activated(a));

    // Commits of a hidden window must not disturb anything.
    compositor.chord(SUPER, KEY_2);
    client.sync();
    client.draw(a, BLUE);
    assert_eq!(compositor.frames().len(), 0);
    compositor.chord(SUPER, KEY_1);
    assert_eq!(compositor.screenshot().at(250, 300), BLUE);
}

#[test]
fn a_window_whose_first_buffer_arrives_on_a_hidden_workspace_takes_no_focus() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_unmapped_window();
    compositor.chord(SUPER, KEY_2);
    let b = client.open_window(GREEN);
    client.draw(a, RED);
    client.sync();
    assert!(client.activated(b));
    assert!(!client.activated(a));

    compositor.chord(SUPER, KEY_1);
    client.sync();
    assert!(client.activated(a), "it should be next in line there");
}

#[test]
fn workspace_prev_and_next_wrap_around() {
    let compositor = Compositor::start();
    compositor.chord(SUPER, KEY_LEFT);
    assert_eq!(compositor.active_workspace(), 8);
    compositor.chord(SUPER, KEY_RIGHT);
    assert_eq!(compositor.active_workspace(), 0);
}

#[test]
fn move_to_workspace_takes_the_window_along() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(SUPER_SHIFT, KEY_3);
    client.sync();
    assert_eq!(compositor.active_workspace(), 2);
    assert_eq!(client.size(b), FULL);
    assert!(client.activated(b));

    compositor.chord(SUPER, KEY_1);
    client.sync();
    assert_eq!(client.size(a), FULL);
    assert!(client.activated(a));

    // Into its own workspace it goes nowhere.
    compositor.chord(SUPER_SHIFT, KEY_1);
    assert_eq!(compositor.active_workspace(), 0);
    assert_eq!(compositor.frames().len(), 1);
}

#[test]
fn float_toggles_out_of_and_back_into_the_tree() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, 12); // ß
    client.sync();
    assert_eq!(client.size(a), FULL, "the tile should take the free space");
    assert_eq!(is_tiled(&compositor), [true, false]);
    // Its width is below the limit of 3/4 of the output and stays, its height is cut.
    assert_eq!(client.size(b), (RIGHT.0, 450));
    client.draw(b, GREEN);
    let frame = compositor.focused_rect().unwrap();
    assert_eq!((frame.w, frame.h), (RIGHT.0, 450));
    assert_eq!(frame.center(), (500, 300));

    compositor.chord(ALT, 12);
    client.sync();
    assert_eq!(is_tiled(&compositor), [true, true]);
    assert_eq!(client.size(a), LEFT);
    assert_eq!(client.size(b), RIGHT);
}

#[test]
fn a_big_window_shrinks_when_it_starts_floating() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.chord(ALT, 12);
    client.sync();
    assert_eq!(client.size(a), (750, 450));
}

#[test]
fn floating_windows_stay_above_tiles_and_cover_their_borders() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, 12);
    client.sync();
    client.draw(a, RED);
    client.draw(b, GREEN);

    // Focusing the tile must not bury the floating window.
    compositor.chord(SUPER, KEY_TAB);
    client.sync();
    assert!(client.activated(a));
    let shot = compositor.screenshot();
    assert_eq!(shot.at(500, 300), GREEN);

    // The border of the tile runs along the bottom, the floating window must hide it if it
    // reaches there. Its own border is drawn above the tile.
    let frame = compositor.frames()[1];
    assert_eq!(
        shot.at(frame.x - 2, 300),
        rgb(Config::default().border_color)
    );
}

#[test]
fn dialogs_and_fixed_size_windows_float_centred() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let main = client.open_window(RED);
    let parent = client.toplevel(main);
    let dialog = client.open_sized_window((300, 200), GREEN, |toplevel| {
        toplevel.set_parent(Some(&parent));
    });
    client.sync();
    assert_eq!(client.size(main), FULL, "the dialog should not be tiled");
    assert!(client.activated(dialog));
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(350, 200, 300, 200))
    );
    assert_eq!(compositor.screenshot().at(500, 300), GREEN);
    assert_eq!(
        compositor.screenshot().at(348, 300),
        rgb(Config::default().focus_color)
    );

    let fixed = client.open_sized_window((100, 100), BLUE, |toplevel| {
        toplevel.set_min_size(100, 100);
        toplevel.set_max_size(100, 100);
    });
    client.sync();
    assert_eq!(client.size(main), FULL);
    assert!(client.activated(fixed));
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(450, 250, 100, 100))
    );
}

#[test]
fn a_floating_window_may_resize_itself() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let main = client.open_window(RED);
    let parent = client.toplevel(main);
    let dialog = client.open_sized_window((300, 200), GREEN, |toplevel| {
        toplevel.set_parent(Some(&parent));
    });
    client.draw_sized(dialog, (400, 100), GREEN);
    client.sync();
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(350, 200, 400, 100))
    );
    let shot = compositor.screenshot();
    assert_eq!(shot.at(748, 250), GREEN);
    assert_eq!(shot.at(752, 250), rgb(Config::default().focus_color));
    // A later retile must not force the old size back on it.
    compositor.with(|state| state.retile());
    client.sync();
    assert_eq!(client.size(dialog), (400, 100));
}

#[test]
fn fullscreen_covers_the_output_and_hides_the_rest() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, KEY_F);
    client.sync();
    assert_eq!(client.size(b), (WIDTH, HEIGHT));
    assert!(client.has_state(b, xdg_toplevel::State::Fullscreen));
    client.draw(b, GREEN);
    assert_eq!(compositor.frames().len(), 1);
    assert_eq!(compositor.with(|state| state.borders.len()), 0);
    let shot = compositor.screenshot();
    for (x, y) in [(0, 0), (999, 599), (500, 300), (250, 300)] {
        assert_eq!(shot.at(x, y), GREEN, "{x},{y}");
    }

    compositor.chord(ALT, KEY_F);
    client.sync();
    assert_eq!(client.size(b), RIGHT);
    assert_eq!(client.size(a), LEFT);
    assert!(!client.has_state(b, xdg_toplevel::State::Fullscreen));
    assert_eq!(compositor.frames().len(), 2);
}

#[test]
fn maximize_fills_the_output_inside_the_outer_gap() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, KEY_A);
    client.sync();
    assert_eq!(client.size(b), FULL);
    assert!(client.has_state(b, xdg_toplevel::State::Maximized));
    client.draw(b, GREEN);
    let shot = compositor.screenshot();
    assert_eq!(shot.at(500, 300), GREEN);
    assert_eq!(shot.at(12, 12), rgb(Config::default().focus_color));
    assert_eq!(shot.at(5, 5), rgb(Config::default().background_color));

    // Fullscreen takes over from maximize and is left in one step.
    compositor.chord(ALT, KEY_F);
    client.sync();
    assert_eq!(client.size(b), (WIDTH, HEIGHT));
    assert!(!client.has_state(b, xdg_toplevel::State::Maximized));
    compositor.chord(ALT, KEY_F);
    client.sync();
    assert_eq!(client.size(b), RIGHT);
    assert_eq!(client.size(a), LEFT);
}

#[test]
fn clients_can_ask_for_fullscreen_and_maximize() {
    let (_compositor, mut client, a, b) = two_windows();
    client.toplevel(a).set_fullscreen(None);
    client.sync();
    assert_eq!(client.size(a), (WIDTH, HEIGHT));
    assert!(client.activated(a), "the covering window needs the focus");
    assert!(!client.activated(b));

    client.toplevel(a).unset_fullscreen();
    client.sync();
    assert_eq!(client.size(a), LEFT);

    client.toplevel(b).set_maximized();
    client.sync();
    assert_eq!(client.size(b), FULL);
    assert!(client.has_state(b, xdg_toplevel::State::Maximized));
    client.toplevel(b).unset_maximized();
    client.sync();
    assert_eq!(client.size(b), RIGHT);

    // A redundant request still gets its configure.
    let before = client.state.windows[b].configures;
    client.toplevel(b).unset_maximized();
    client.sync();
    assert!(client.state.windows[b].configures > before);
}

#[test]
fn a_window_can_start_fullscreen() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_unmapped_window_with(|toplevel| toplevel.set_fullscreen(None));
    assert_eq!(client.size(b), (WIDTH, HEIGHT));
    assert!(client.has_state(b, xdg_toplevel::State::Fullscreen));
    client.draw(b, GREEN);
    client.sync();
    assert!(client.activated(b));
    assert_eq!(compositor.frames().len(), 1);

    client.toplevel(b).unset_fullscreen();
    client.sync();
    assert_eq!(client.size(a), LEFT);
}

#[test]
fn a_new_window_ends_fullscreen() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, KEY_F);
    client.sync();
    let c = client.open_window(BLUE);
    client.sync();
    assert!(client.activated(c));
    assert_eq!(compositor.frames().len(), 3);
    assert_eq!(client.size(a), LEFT);
    assert!(!client.has_state(b, xdg_toplevel::State::Fullscreen));
}

#[test]
fn closing_a_fullscreen_window_brings_the_others_back() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, KEY_F);
    client.sync();
    client.close(b);
    client.sync();
    assert_eq!(client.size(a), FULL);
    assert!(client.activated(a));
    assert_eq!(compositor.frames().len(), 1);
}

#[test]
fn focus_next_leaves_fullscreen_instead_of_focusing_a_hidden_window() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, KEY_F);
    client.sync();
    compositor.chord(SUPER, KEY_TAB);
    client.sync();
    assert!(client.activated(a));
    assert_eq!(compositor.frames().len(), 2);
    assert_eq!(client.size(b), RIGHT);
}

#[test]
fn fullscreen_is_per_workspace() {
    let (compositor, mut client, a, b) = two_windows();
    compositor.chord(ALT, KEY_F);
    client.sync();
    compositor.chord(SUPER, KEY_2);
    assert!(compositor.frames().is_empty());
    let c = client.open_window(BLUE);
    assert_eq!(client.size(c), FULL);
    compositor.chord(SUPER, KEY_1);
    client.sync();
    assert_eq!(compositor.frames().len(), 1);
    assert_eq!(client.size(b), (WIDTH, HEIGHT));
    assert!(client.activated(b));
    let _ = a;
}

/// Holds Super and drags with `button` from one point to another.
fn super_drag(compositor: &Compositor, button: u32, from: (f64, f64), to: (f64, f64)) {
    compositor.pointer_at(from.0, from.1);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(button, ButtonState::Pressed);
    compositor.pointer_at(to.0, to.1);
    compositor.button(button, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
}

/// A window of 300x200 floating over `main`, at 350,200.
fn floating_dialog() -> (Compositor, Client, usize, usize) {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let main = client.open_window(RED);
    let parent = client.toplevel(main);
    let dialog = client.open_sized_window((300, 200), GREEN, |toplevel| {
        toplevel.set_parent(Some(&parent));
    });
    client.sync();
    client.take_input();
    (compositor, client, main, dialog)
}

#[test]
fn super_left_drag_moves_a_floating_window_and_hides_the_press() {
    let (compositor, mut client, _, _) = floating_dialog();
    super_drag(&compositor, BTN_LEFT, (400.0, 300.0), (500.0, 350.0));
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(450, 250, 300, 200))
    );
    assert!(
        !client
            .take_input()
            .iter()
            .any(|input| matches!(input, Input::Button(..)))
    );
    assert_eq!(compositor.screenshot().at(700, 400), GREEN);
}

#[test]
fn the_pointer_returns_to_the_client_after_a_drag() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    super_drag(&compositor, BTN_LEFT, (400.0, 300.0), (500.0, 350.0));
    assert!(client.take_input().contains(&Input::PointerLeave(dialog)));
    compositor.pointer_at(510.0, 360.0);
    compositor.click();
    assert!(
        client
            .take_input()
            .contains(&Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed))
    );
}

#[test]
fn super_right_drag_resizes_from_the_nearest_corner() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    // Bottom right quarter: the top left corner stays.
    super_drag(&compositor, BTN_RIGHT, (600.0, 350.0), (650.0, 400.0));
    client.sync();
    assert_eq!(client.size(dialog), (350, 250));
    client.draw(dialog, GREEN);
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(350, 200, 350, 250))
    );

    // Top left quarter: the bottom right corner stays.
    super_drag(&compositor, BTN_RIGHT, (400.0, 250.0), (380.0, 220.0));
    client.sync();
    assert_eq!(client.size(dialog), (370, 280));
    client.draw(dialog, GREEN);
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(330, 170, 370, 280))
    );
}

#[test]
fn resizing_tells_the_client_while_it_lasts() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    compositor.pointer_at(600.0, 350.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_RIGHT, ButtonState::Pressed);
    client.sync();
    assert!(client.has_state(dialog, xdg_toplevel::State::Resizing));
    compositor.button(BTN_RIGHT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    client.sync();
    assert!(!client.has_state(dialog, xdg_toplevel::State::Resizing));
}

#[test]
fn resizing_stops_at_the_minimum_size() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let main = client.open_window(RED);
    let parent = client.toplevel(main);
    let dialog = client.open_sized_window((300, 200), GREEN, |toplevel| {
        toplevel.set_parent(Some(&parent));
        toplevel.set_min_size(200, 150);
    });
    client.sync();
    super_drag(&compositor, BTN_RIGHT, (600.0, 350.0), (100.0, 50.0));
    client.sync();
    assert_eq!(client.size(dialog), (200, 150));
}

#[test]
fn super_right_drag_on_a_tile_moves_the_divider() {
    let (compositor, mut client, a, b) = two_windows();
    super_drag(&compositor, BTN_RIGHT, (750.0, 300.0), (650.0, 300.0));
    client.sync();
    assert_eq!(client.size(a), (LEFT.0 - 100, LEFT.1));
    assert_eq!(client.size(b), (RIGHT.0 + 100, RIGHT.1));
    assert!(compositor.with(|state| state.drag.is_none()));
}

#[test]
fn dragging_a_tile_onto_another_swaps_them() {
    let (compositor, mut client, a, b) = two_windows();
    super_drag(&compositor, BTN_LEFT, (250.0, 300.0), (750.0, 300.0));
    client.sync();
    assert_eq!(client.size(a), RIGHT);
    assert_eq!(client.size(b), LEFT);
    assert!(client.activated(a));
}

#[test]
fn dropping_a_tile_on_itself_changes_nothing() {
    let (compositor, mut client, a, b) = two_windows();
    super_drag(&compositor, BTN_LEFT, (250.0, 300.0), (260.0, 310.0));
    client.sync();
    assert_eq!(client.size(a), LEFT);
    assert_eq!(client.size(b), RIGHT);
}

#[test]
fn a_fullscreen_window_cannot_be_dragged() {
    let (compositor, mut client, _, b) = two_windows();
    compositor.chord(ALT, KEY_F);
    client.sync();
    super_drag(&compositor, BTN_LEFT, (250.0, 300.0), (750.0, 300.0));
    client.sync();
    assert_eq!(client.size(b), (WIDTH, HEIGHT));
    assert!(compositor.with(|state| state.drag.is_none()));
}

#[test]
fn a_window_closing_during_a_drag_ends_it_quietly() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    compositor.pointer_at(400.0, 300.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    client.close(dialog);
    compositor.pointer_at(500.0, 350.0);
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    assert!(compositor.with(|state| state.drag.is_none()));
    compositor.pointer_at(510.0, 350.0);
    compositor.click();
}

#[test]
fn a_client_can_start_a_move_and_a_resize_by_itself() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    let seat = client.seat();

    // As a title bar would: press, then ask to move.
    compositor.pointer_at(400.0, 300.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    client
        .toplevel(dialog)
        ._move(&seat, client.state.button_serial);
    client.sync();
    assert!(compositor.with(|state| state.drag.is_some()));
    compositor.pointer_at(450.0, 310.0);
    compositor.button(BTN_LEFT, ButtonState::Released);
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(400, 210, 300, 200))
    );

    // As a left border would.
    compositor.pointer_at(405.0, 300.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    client.toplevel(dialog).resize(
        &seat,
        client.state.button_serial,
        xdg_toplevel::ResizeEdge::Left,
    );
    client.sync();
    compositor.pointer_at(375.0, 300.0);
    compositor.button(BTN_LEFT, ButtonState::Released);
    client.sync();
    assert_eq!(client.size(dialog), (330, 200));
    client.draw(dialog, GREEN);
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(370, 210, 330, 200))
    );
}

#[test]
fn a_move_request_with_a_stale_serial_is_ignored() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    let seat = client.seat();
    client.toplevel(dialog)._move(&seat, 1);
    client.sync();
    assert!(compositor.with(|state| state.drag.is_none()));
}

/// The defaults: sloppy focus and warping on.
fn mango_like() -> (Compositor, Client, usize, usize) {
    let compositor = Compositor::start_with("");
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    client.take_input();
    (compositor, client, a, b)
}

#[test]
fn sloppy_focus_follows_the_pointer_between_windows() {
    let (compositor, mut client, a, b) = mango_like();
    assert!(client.activated(b));
    compositor.pointer_at(250.0, 300.0);
    client.sync();
    assert!(client.activated(a));
    compositor.pointer_at(750.0, 300.0);
    client.sync();
    assert!(client.activated(b));
}

#[test]
fn sloppy_focus_keeps_focus_on_the_background() {
    let (compositor, mut client, _, b) = mango_like();
    compositor.pointer_at(2.0, 2.0);
    client.sync();
    assert!(client.activated(b));
}

#[test]
fn sloppy_focus_needs_motion_and_not_just_a_window_appearing() {
    let (compositor, mut client, a, _) = mango_like();
    compositor.pointer_at(250.0, 300.0);
    client.sync();
    assert!(client.activated(a));
    // Opening a window focuses it, and the pointer resting elsewhere does not take that back.
    let c = client.open_window(BLUE);
    client.sync();
    assert!(client.activated(c));
    compositor.with(|state| state.retile());
    client.sync();
    assert!(client.activated(c));
}

#[test]
fn sloppy_focus_does_not_raise_but_a_click_does() {
    let compositor = Compositor::start_with("");
    let mut client = Client::connect(&compositor);
    let main = client.open_window(RED);
    let parent = client.toplevel(main);
    let first = client.open_sized_window((300, 300), GREEN, |toplevel| {
        toplevel.set_parent(Some(&parent));
    });
    let second = client.open_sized_window((300, 300), BLUE, |toplevel| {
        toplevel.set_parent(Some(&parent));
    });
    // Same size and centre: only move the second one aside to see both.
    compositor.with(|state| {
        let workspace = state.monitors[0].workspace_mut();
        workspace.floating[1].rect.x += 150;
        state.retile();
    });
    let shot = compositor.screenshot();
    assert_eq!(shot.at(500, 300), BLUE);

    // The left part of the lower window is uncovered. Hovering focuses, but keeps the order.
    compositor.pointer_at(400.0, 300.0);
    client.sync();
    assert!(client.activated(first));
    assert_eq!(compositor.screenshot().at(500, 300), BLUE);

    compositor.click();
    client.sync();
    assert_eq!(compositor.screenshot().at(500, 300), GREEN);
    let _ = second;
}

#[test]
fn a_held_button_stops_sloppy_focus() {
    let (compositor, mut client, a, b) = mango_like();
    compositor.pointer_at(250.0, 300.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    compositor.pointer_at(750.0, 300.0);
    client.sync();
    assert!(client.activated(a), "a text selection must not lose focus");
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.pointer_at(751.0, 300.0);
    client.sync();
    assert!(client.activated(b));
}

#[test]
fn keyboard_focus_changes_warp_the_pointer_like_mango() {
    let (compositor, mut client, a, _) = mango_like();
    compositor.pointer_at(750.0, 300.0);
    compositor.chord(ALT, KEY_LEFT);
    client.sync();
    assert!(client.activated(a));
    let location = compositor.with(|state| {
        let pointer = state.seat.get_pointer().unwrap();
        pointer.current_location()
    });
    assert_eq!((location.x, location.y), (254.0, 300.0));
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(a, 240.0, 286.0))
    );
}

#[test]
fn warping_can_be_turned_off() {
    let compositor = Compositor::start_with("warp_cursor = 0\n");
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_window(GREEN);
    compositor.pointer_at(750.0, 300.0);
    compositor.chord(ALT, KEY_LEFT);
    let location = compositor.with(|state| {
        let pointer = state.seat.get_pointer().unwrap();
        pointer.current_location()
    });
    assert_eq!((location.x, location.y), (750.0, 300.0));
}

/// Two 1000x600 outputs side by side, the second one to the right.
fn two_outputs() -> (Compositor, Output) {
    let compositor = Compositor::start();
    let second = compositor.add_output("second", (WIDTH, 0));
    (compositor, second)
}

#[test]
fn each_output_tiles_its_own_windows() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);

    compositor.pointer_at(1500.0, 300.0);
    let c = client.open_window(BLUE);
    client.sync();
    assert_eq!(client.size(c), FULL);
    assert_eq!(client.size(a), LEFT);
    assert_eq!(client.size(b), RIGHT);
    assert_eq!(
        compositor.with(|state| state.monitors[1].workspace().windows().len()),
        1
    );
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(1014, 14, 972, 572))
    );

    assert_eq!(compositor.screenshot_of(0).at(250, 300), RED);
    assert_eq!(compositor.screenshot_of(1).at(500, 300), BLUE);
    assert_eq!(
        compositor.screenshot_of(1).at(2, 2),
        rgb(Config::default().background_color)
    );
}

#[test]
fn workspaces_are_per_output() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.pointer_at(1500.0, 300.0);
    let b = client.open_window(GREEN);

    // Output 2 goes to workspace 2, output 1 stays where it is.
    compositor.chord(SUPER, KEY_2);
    client.sync();
    assert_eq!(compositor.frames(), [Rect::new(14, 14, 972, 572)]);
    assert_eq!(compositor.with(|state| state.monitors[0].active), 0);
    assert_eq!(compositor.with(|state| state.monitors[1].active), 1);
    assert_eq!(compositor.focused_rect(), None);

    compositor.chord(SUPER, KEY_1);
    client.sync();
    assert!(client.activated(b));
    assert!(!client.activated(a));
}

#[test]
fn popups_stay_on_the_output_of_their_window() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    compositor.pointer_at(1500.0, 300.0);
    let a = client.open_window(RED);
    let _popup = client.open_edge_popup(a);
    // Measured against the first output it would have room to stay at 961.
    assert_eq!(client.state.popup_position, Some((886, 21)));
}

#[test]
fn focus_and_windows_move_between_outputs() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);

    let alt_shift = &[KEY_LEFTALT, KEY_LEFTSHIFT];
    compositor.chord(alt_shift, KEY_RIGHT);
    client.sync();
    assert_eq!(compositor.with(|state| state.active_monitor), 1);
    assert_eq!(compositor.focused_rect(), None);
    let location = compositor.with(|state| state.seat.get_pointer().unwrap().current_location());
    assert_eq!((location.x, location.y), (1500.0, 300.0));
    compositor.chord(alt_shift, KEY_LEFT);
    client.sync();
    assert_eq!(compositor.with(|state| state.active_monitor), 0);
    assert!(client.activated(b));

    compositor.chord(&[KEY_LEFTMETA, KEY_LEFTALT], KEY_RIGHT);
    client.sync();
    assert_eq!(client.size(a), FULL);
    assert_eq!(client.size(b), FULL);
    assert!(client.activated(b));
    assert_eq!(compositor.with(|state| state.active_monitor), 1);
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(1014, 14, 972, 572))
    );

    // Nothing lies further right.
    compositor.chord(&[KEY_LEFTMETA, KEY_LEFTALT], KEY_RIGHT);
    assert_eq!(compositor.with(|state| state.active_monitor), 1);
}

#[test]
fn a_floating_window_dragged_across_outputs_changes_workspace() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    let main = client.open_window(RED);
    let parent = client.toplevel(main);
    let _dialog = client.open_sized_window((300, 200), GREEN, |toplevel| {
        toplevel.set_parent(Some(&parent));
    });
    client.sync();

    super_drag(&compositor, BTN_LEFT, (400.0, 300.0), (1400.0, 300.0));
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(1350, 200, 300, 200))
    );
    assert_eq!(
        compositor.with(|state| state.monitors[1].workspace().floating.len()),
        1
    );
    assert_eq!(
        compositor.with(|state| state.monitors[0].workspace().floating.len()),
        0
    );
    assert_eq!(compositor.screenshot_of(1).at(500, 300), GREEN);
}

#[test]
fn a_tile_dropped_on_another_output_joins_its_tree() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    super_drag(&compositor, BTN_LEFT, (750.0, 300.0), (1500.0, 300.0));
    client.sync();
    assert_eq!(client.size(a), FULL);
    assert_eq!(client.size(b), FULL);
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(1014, 14, 972, 572))
    );
}

#[test]
fn an_unplugged_output_hands_over_its_windows() {
    let (compositor, second) = two_outputs();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.pointer_at(1500.0, 300.0);
    let b = client.open_window(GREEN);
    compositor.chord(SUPER_SHIFT, KEY_2);
    client.sync();
    let c = client.open_window(BLUE);
    client.sync();

    compositor.with(move |state| state.remove_output(&second));
    client.sync();
    assert_eq!(compositor.with(|state| state.monitors.len()), 1);
    assert_eq!(compositor.with(|state| state.active_monitor), 0);
    assert_eq!(client.size(a), FULL);
    assert!(
        client.activated(a),
        "the focused window went to a hidden workspace"
    );
    assert!(!client.activated(b));
    assert!(!client.activated(c));
    // The pointer was left on the vanished output.
    let location = compositor.with(|state| state.seat.get_pointer().unwrap().current_location());
    assert_eq!((location.x, location.y), (500.0, 300.0));

    compositor.chord(SUPER, KEY_2);
    client.sync();
    assert_eq!(compositor.frames().len(), 2);
}

#[test]
fn windows_survive_the_last_output_going_away() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    let output = compositor.with(|state| state.monitors[0].output.clone());
    compositor.with(move |state| state.remove_output(&output));
    client.sync();
    assert_eq!(compositor.frames().len(), 0);

    // A commit in this state must not crash, and new windows wait for an output.
    client.draw(a, RED);
    assert_eq!(compositor.with(|state| state.frames.len()), 0);

    compositor.add_output("new", (0, 0));
    client.sync();
    assert_eq!(compositor.frames().len(), 2);
    assert_eq!(client.size(a), LEFT);
    assert_eq!(client.size(b), RIGHT);
    assert!(client.activated(b));
    assert_eq!(compositor.screenshot().at(750, 300), GREEN);
}

#[test]
fn a_shrinking_output_pulls_floating_windows_back_in() {
    let (compositor, mut client, _, _) = floating_dialog();
    super_drag(&compositor, BTN_LEFT, (400.0, 300.0), (900.0, 500.0));
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let mode = Mode {
            size: (600, 400).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        state.output_resized(&output);
    });
    client.sync();
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(300, 200, 300, 200))
    );
}

#[test]
fn surface_coordinates_are_relative_on_the_second_output() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    compositor.pointer_at(1500.0, 300.0);
    client.open_window(RED);
    client.take_input();
    compositor.pointer_at(1600.0, 200.0);
    let input = client.take_input();
    assert!(
        input.contains(&Input::PointerMotion(586.0, 186.0)),
        "{input:?}"
    );
}

fn scroll(
    compositor: &Compositor,
    build: impl FnOnce(&mut Scroll) + Send + 'static,
    source: AxisSource,
) {
    compositor.with(move |state| {
        let mut scroll = Scroll::new(source, InputTime::now());
        build(&mut scroll);
        state.on_pointer_axis(scroll);
    });
}

#[test]
fn a_wheel_scrolls_with_value_and_notches_on_both_axes() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.pointer_at(250.0, 300.0);
    client.take_input();

    scroll(
        &compositor,
        |scroll| scroll.set(Axis::Vertical, None, Some(120.0)),
        AxisSource::Wheel,
    );
    assert_eq!(
        client.take_input(),
        [
            Input::AxisSource(wl_pointer::AxisSource::Wheel),
            Input::AxisV120(wl_pointer::Axis::VerticalScroll, 120),
            Input::Axis(wl_pointer::Axis::VerticalScroll, 15.0),
        ]
    );

    scroll(
        &compositor,
        |scroll| scroll.set(Axis::Horizontal, None, Some(-60.0)),
        AxisSource::Wheel,
    );
    assert_eq!(
        client.take_input(),
        [
            Input::AxisSource(wl_pointer::AxisSource::Wheel),
            Input::AxisV120(wl_pointer::Axis::HorizontalScroll, -60),
            Input::Axis(wl_pointer::Axis::HorizontalScroll, -7.5),
        ]
    );
}

#[test]
fn a_touchpad_scrolls_smoothly_and_stops() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.pointer_at(250.0, 300.0);
    client.take_input();

    scroll(
        &compositor,
        |scroll| {
            scroll.set(Axis::Horizontal, Some(1.5), None);
            scroll.set(Axis::Vertical, Some(-2.5), None);
        },
        AxisSource::Finger,
    );
    let input = client.take_input();
    assert!(input.contains(&Input::AxisSource(wl_pointer::AxisSource::Finger)));
    assert!(input.contains(&Input::Axis(wl_pointer::Axis::HorizontalScroll, 1.5)));
    assert!(input.contains(&Input::Axis(wl_pointer::Axis::VerticalScroll, -2.5)));

    scroll(
        &compositor,
        |scroll| {
            scroll.set(Axis::Horizontal, Some(0.0), None);
            scroll.set(Axis::Vertical, Some(0.0), None);
        },
        AxisSource::Finger,
    );
    let input = client.take_input();
    assert!(input.contains(&Input::AxisStop(wl_pointer::Axis::HorizontalScroll)));
    assert!(input.contains(&Input::AxisStop(wl_pointer::Axis::VerticalScroll)));
}

#[test]
fn scrolling_reaches_the_window_under_the_pointer_only() {
    let (compositor, mut client, _, _) = two_windows();
    compositor.pointer_at(2.0, 2.0);
    client.take_input();
    scroll(
        &compositor,
        |scroll| scroll.set(Axis::Vertical, None, Some(120.0)),
        AxisSource::Wheel,
    );
    assert_eq!(client.take_input(), []);
}

#[test]
fn stacking_order_survives_a_trip_through_a_hidden_workspace() {
    let (compositor, _client, _, _) = floating_dialog();
    compositor.chord(SUPER, KEY_2);
    compositor.chord(SUPER, KEY_1);
    assert_eq!(compositor.screenshot().at(500, 300), GREEN);
    let placements = compositor.with(|state| {
        let workspace = state.monitors[0].workspace();
        workspace
            .windows()
            .iter()
            .map(|window| workspace.placement(window))
            .collect::<Vec<_>>()
    });
    assert_eq!(placements[0], Some(Placement::Tiled));
    assert!(matches!(placements[1], Some(Placement::Floating(_))));
}
