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
use wirbel::{
    Wirbel,
    state::{BACKGROUND_COLOR, BORDER_COLOR, FOCUS_COLOR},
};

const WIDTH: i32 = 1000;
const HEIGHT: i32 = 600;
const BTN_LEFT: u32 = 0x110;

type Job = Box<dyn FnOnce(&mut Wirbel) + Send>;

struct Compositor {
    jobs: channel::Sender<Job>,
    socket: PathBuf,
    thread: Option<JoinHandle<()>>,
}

impl Compositor {
    fn start() -> Self {
        let (jobs, job_source) = channel::channel::<Job>();
        let (socket_tx, socket_rx) = mpsc::channel();

        let thread = thread::spawn(move || {
            let mut event_loop = EventLoop::try_new().unwrap();
            let display = Display::new().unwrap();
            let mut state = Wirbel::new(&mut event_loop, display).unwrap();

            let output = test_output();
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

    fn tap_key(&self, evdev: u32) {
        self.with(move |state| {
            let keycode = (evdev + 8).into();
            state.on_key(keycode, KeyState::Pressed, InputTime::now());
            state.on_key(keycode, KeyState::Released, InputTime::now());
        });
    }

    /// Renders the output with pixman into XRGB pixels.
    fn screenshot(&self) -> Screenshot {
        let pixels = self.with(|state| {
            let output = state.space.outputs().next().unwrap().clone();
            let size = Size::<i32, Buffer>::from((WIDTH, HEIGHT));

            let mut renderer = PixmanRenderer::new().unwrap();
            let mut image: Image<'static, 'static> =
                renderer.create_buffer(Fourcc::Argb8888, size).unwrap();
            let mut framebuffer = renderer.bind(&mut image).unwrap();

            let elements = state.scene_elements(&mut renderer, &output);
            OutputDamageTracker::from_output(&output)
                .render_output(
                    &mut renderer,
                    &mut framebuffer,
                    0,
                    &elements,
                    BACKGROUND_COLOR,
                )
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

fn test_output() -> Output {
    let output = Output::new(
        "test".to_string(),
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
        Some((0, 0).into()),
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
}

#[derive(Default)]
struct ClientState {
    windows: Vec<WindowState>,
    input: Vec<Input>,
    /// Position of the latest popup configure, relative to the parent window.
    popup_position: Option<(i32, i32)>,
}

struct WindowState {
    surface: WlSurface,
    xdg_surface: XdgSurface,
    toplevel: XdgToplevel,
    /// Size of the latest configure.
    size: (i32, i32),
    activated: bool,
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
        let surface = self.compositor.create_surface(&self.handle, ());
        let xdg_surface = self.wm_base.get_xdg_surface(&surface, &self.handle, ());
        let toplevel = xdg_surface.get_toplevel(&self.handle, ());
        surface.commit();
        self.state.windows.push(WindowState {
            surface,
            xdg_surface,
            toplevel,
            size: (0, 0),
            activated: false,
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

    /// Fills the window with `color`, using the size of the last configure.
    fn draw(&mut self, index: usize, color: u32) {
        let (width, height) = self.state.windows[index].size;
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
        if let xdg_toplevel::Event::Configure {
            width,
            height,
            states,
        } = event
            && let Some(window) = state.windows.iter_mut().find(|w| w.toplevel == *toplevel)
        {
            window.size = (width, height);
            let activated = xdg_toplevel::State::Activated as u32;
            window.activated = states
                .chunks_exact(4)
                .any(|chunk| u32::from_ne_bytes(chunk.try_into().unwrap()) == activated);
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
                state: WEnum::Value(button_state),
                ..
            } => Input::Button(button, button_state),
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
    assert!(compositor.with(|state| state.layout.items().len() == 1));
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
        let output = state.space.outputs().next().unwrap().clone();
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
    assert!(compositor.with(|state| state.layout.items().len() == 1));

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
        assert!(compositor.with(|state| state.layout.items().len() == 3));
    }
    survivor.sync();
    // The server sees the hangup asynchronously.
    for _ in 0..100 {
        if compositor.with(|state| state.layout.items().len() == 1) {
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

    // Anchored at the right edge of the window, so the popup would stick out of the output.
    let positioner = client.wm_base.create_positioner(&client.handle, ());
    positioner.set_size(100, 50);
    positioner.set_anchor_rect(960, 20, 1, 1);
    positioner.set_anchor(xdg_positioner::Anchor::BottomRight);
    positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
    positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::SlideX);

    let surface = client.compositor.create_surface(&client.handle, ());
    let xdg_surface = client.wm_base.get_xdg_surface(&surface, &client.handle, ());
    let parent = client.state.windows[a].xdg_surface.clone();
    let _popup = xdg_surface.get_popup(Some(&parent), &positioner, &client.handle, ());
    surface.commit();
    client.sync();

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
    let background = rgb(BACKGROUND_COLOR);
    let border = rgb(BORDER_COLOR);
    let focus = rgb(FOCUS_COLOR);

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
    assert_eq!(shot.at(10, 300), rgb(FOCUS_COLOR));
    assert_eq!(shot.at(503, 300), rgb(BORDER_COLOR));
}
