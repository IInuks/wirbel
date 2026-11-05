//! Runs the compositor without a backend and talks to it as a real Wayland client. Input is
//! injected where a backend would deliver it, output is rendered with pixman.

use std::{
    env,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
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
use smithay::{
    reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback as server_feedback,
    wayland::{pointer_constraints::with_pointer_constraint, presentation::Refresh},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    event_created_child,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::WlDataDeviceManager,
        wl_data_offer::WlDataOffer,
        wl_data_source::{self, WlDataSource},
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_region::WlRegion,
        wl_registry::WlRegistry,
        wl_seat::WlSeat,
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_confined_pointer_v1::{self, ZwpConfinedPointerV1},
    zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
    zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
};
use wayland_protocols::wp::presentation_time::client::{
    wp_presentation::WpPresentation,
    wp_presentation_feedback::{self, WpPresentationFeedback},
};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
    zwp_primary_selection_device_v1::{self, ZwpPrimarySelectionDeviceV1},
    zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
    zwp_primary_selection_source_v1::{self, ZwpPrimarySelectionSourceV1},
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
    zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::XdgActivationV1,
};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup::{self, XdgPopup},
    xdg_positioner::{self, XdgPositioner},
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};
use wirbel::{
    Wirbel, config::Config, focus::KeyboardFocus, input::Scroll, ipc::Position, layout::Rect,
    workspace::Placement,
};
use x11rb::{
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT,
    connection::Connection as _,
    protocol::{
        Event,
        xproto::{
            AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux,
            EventMask, GrabMode, GrabStatus, PropMode, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

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

    /// What the backend does after it presented a frame on every output.
    fn present(&self) {
        self.with(|state| {
            let outputs = state.monitors.iter().map(|monitor| monitor.output.clone());
            for output in outputs.collect::<Vec<_>>() {
                state.send_frame_callbacks(&output);
            }
        });
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
    /// Surfaces that are no windows. In input events they count from `OTHER_SURFACES` on.
    others: Vec<WlSurface>,
    layers: Vec<LayerState>,
    popups_done: u32,
    lock_events: Vec<LockEvent>,
    /// Size of the latest configure of a lock surface.
    lock_size: Option<(u32, u32)>,
    decoration_modes: Vec<zxdg_toplevel_decoration_v1::Mode>,
    offers: Vec<WlDataOffer>,
    selection: Option<WlDataOffer>,
    primary_offers: Vec<ZwpPrimarySelectionOfferV1>,
    primary: Option<ZwpPrimarySelectionOfferV1>,
    dnd: Vec<DndEvent>,
    source_events: Vec<SourceEvent>,
    preferred_scale: Option<u32>,
    idle: Vec<bool>,
    dmabuf: Option<Result<(), ()>>,
    logical_size: Option<(i32, i32)>,
    /// Serial of the latest key event, for the clipboard.
    key_serial: u32,
    /// Serial of the latest pointer enter, for setting the cursor.
    enter_serial: u32,
    /// Lock and confinement going on and off.
    constraint_events: Vec<bool>,
    /// Accelerated and raw motion by the relative pointer.
    relative: Vec<(f64, f64, f64, f64)>,
    /// Whether presentation feedback said presented, each time it answered.
    presented: Vec<bool>,
    /// The latest activation token.
    activation_token: Option<String>,
}

/// First index of surfaces in `Input` events that are no windows.
const OTHER_SURFACES: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq)]
enum LockEvent {
    Locked,
    Finished,
}

#[derive(Debug, Clone, PartialEq)]
enum DndEvent {
    Enter(f64, f64),
    Motion(f64, f64),
    Leave,
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SourceEvent {
    Target,
    Dropped,
    Finished,
    Cancelled,
}

struct LayerState {
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    size: (u32, u32),
    configures: u32,
    closed: bool,
    buffer: Option<WlBuffer>,
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

    fn other_index(&self, surface: &WlSurface) -> Option<usize> {
        let others = self.others.iter();
        let index = others.into_iter().position(|other| other == surface)?;
        Some(OTHER_SURFACES + index)
    }

    /// `None` if the client already destroyed the surface. A leave for such a surface crosses
    /// paths with the destroy request and is not an error.
    fn find_window(&self, surface: &WlSurface) -> Option<usize> {
        self.windows
            .iter()
            .position(|window| window.surface == *surface)
            .or_else(|| self.other_index(surface))
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

    /// Remembers `surface` for input events and returns its index there.
    fn track(&mut self, surface: &WlSurface) -> usize {
        self.state.others.push(surface.clone());
        OTHER_SURFACES + self.state.others.len() - 1
    }

    fn bind<I: Proxy + 'static>(&self, versions: std::ops::RangeInclusive<u32>) -> I
    where
        ClientState: Dispatch<I, ()>,
    {
        self.globals.bind(&self.handle, versions, ()).unwrap()
    }

    /// Creates a layer surface and walks it through the initial configure. `setup` runs before
    /// the first commit.
    fn open_unmapped_layer(
        &mut self,
        level: zwlr_layer_shell_v1::Layer,
        setup: impl FnOnce(&ZwlrLayerSurfaceV1),
    ) -> usize {
        let shell: ZwlrLayerShellV1 = self.bind(1..=5);
        let surface = self.compositor.create_surface(&self.handle, ());
        let layer = shell.get_layer_surface(&surface, None, level, "test".into(), &self.handle, ());
        setup(&layer);
        surface.commit();
        self.state.others.push(surface.clone());
        self.state.layers.push(LayerState {
            surface,
            layer,
            size: (0, 0),
            configures: 0,
            closed: false,
            buffer: None,
        });
        self.sync();
        let index = self.state.layers.len() - 1;
        assert_eq!(
            self.state.layers[index].configures, 1,
            "no initial configure"
        );
        index
    }

    fn open_layer(
        &mut self,
        level: zwlr_layer_shell_v1::Layer,
        color: u32,
        setup: impl FnOnce(&ZwlrLayerSurfaceV1),
    ) -> usize {
        let index = self.open_unmapped_layer(level, setup);
        self.draw_layer(index, color);
        index
    }

    /// A bar of the given height along the top of the output.
    fn open_bar(&mut self, height: u32, color: u32) -> usize {
        self.open_layer(zwlr_layer_shell_v1::Layer::Top, color, |layer| {
            layer.set_size(0, height);
            layer.set_anchor(top_bar_anchor());
            layer.set_exclusive_zone(height as i32);
        })
    }

    fn draw_layer(&mut self, index: usize, color: u32) {
        let layer = &self.state.layers[index];
        let surface = layer.surface.clone();
        let (w, h) = (layer.size.0 as i32, layer.size.1 as i32);
        let buffer = self.fill(&surface, w, h, color);
        self.state.layers[index].buffer = Some(buffer);
    }

    fn close_layer(&mut self, index: usize) {
        let layer = &self.state.layers[index];
        layer.layer.destroy();
        layer.surface.destroy();
        self.sync();
    }

    /// Input index of the surface of a layer surface.
    fn layer_input(&self, index: usize) -> usize {
        self.state
            .other_index(&self.state.layers[index].surface)
            .unwrap()
    }

    /// A positioner for a popup of the given size at `(x, y)` of its parent.
    fn positioner(&self, (x, y): (i32, i32), (w, h): (i32, i32)) -> XdgPositioner {
        let positioner = self.wm_base.create_positioner(&self.handle, ());
        positioner.set_size(w, h);
        positioner.set_anchor_rect(x, y, 1, 1);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        positioner
    }

    /// Opens and draws a popup of `parent`, grabbing with `grab` if given.
    fn open_popup(
        &mut self,
        parent: &XdgSurface,
        at: (i32, i32),
        size: (i32, i32),
        color: u32,
        grab: Option<u32>,
    ) -> TestPopup {
        let positioner = self.positioner(at, size);
        let surface = self.compositor.create_surface(&self.handle, ());
        let xdg_surface = self.wm_base.get_xdg_surface(&surface, &self.handle, ());
        let popup = xdg_surface.get_popup(Some(parent), &positioner, &self.handle, ());
        self.finish_popup(surface, xdg_surface, popup, size, color, grab)
    }

    fn finish_popup(
        &mut self,
        surface: WlSurface,
        xdg_surface: XdgSurface,
        popup: XdgPopup,
        size: (i32, i32),
        color: u32,
        grab: Option<u32>,
    ) -> TestPopup {
        if let Some(serial) = grab {
            popup.grab(&self.seat(), serial);
        }
        let input = self.track(&surface);
        surface.commit();
        self.sync();
        let buffer = self.fill(&surface, size.0, size.1, color);
        TestPopup {
            surface,
            xdg_surface,
            popup,
            input,
            _buffer: buffer,
        }
    }

    /// Opens and draws a popup of a layer surface.
    fn open_layer_popup(
        &mut self,
        index: usize,
        at: (i32, i32),
        size: (i32, i32),
        color: u32,
        grab: Option<u32>,
    ) -> TestPopup {
        let positioner = self.positioner(at, size);
        let surface = self.compositor.create_surface(&self.handle, ());
        let xdg_surface = self.wm_base.get_xdg_surface(&surface, &self.handle, ());
        let popup = xdg_surface.get_popup(None, &positioner, &self.handle, ());
        self.state.layers[index].layer.get_popup(&popup);
        self.finish_popup(surface, xdg_surface, popup, size, color, grab)
    }

    fn xdg_surface(&self, index: usize) -> XdgSurface {
        self.state.windows[index].xdg_surface.clone()
    }

    /// Asks for the session lock and returns the lock object.
    fn lock(&mut self) -> ExtSessionLockV1 {
        let manager: ExtSessionLockManagerV1 = self.bind(1..=1);
        let lock = manager.lock(&self.handle, ());
        self.sync();
        lock
    }

    /// Creates a lock surface on the first output of this client and draws it. Returns its
    /// index in input events.
    fn lock_surface(
        &mut self,
        lock: &ExtSessionLockV1,
        output: &WlOutput,
        color: u32,
    ) -> LockedSurface {
        let surface = self.compositor.create_surface(&self.handle, ());
        let input = self.track(&surface);
        let lock_surface = lock.get_lock_surface(&surface, output, &self.handle, ());
        self.sync();
        let (w, h) = self
            .state
            .lock_size
            .expect("no configure for the lock surface");
        let buffer = self.fill(&surface, w as i32, h as i32, color);
        LockedSurface {
            surface,
            lock_surface,
            input,
            _buffer: buffer,
        }
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

struct TestPopup {
    surface: WlSurface,
    xdg_surface: XdgSurface,
    popup: XdgPopup,
    input: usize,
    _buffer: WlBuffer,
}

impl TestPopup {
    fn destroy(&self) {
        self.popup.destroy();
        self.xdg_surface.destroy();
        self.surface.destroy();
    }
}

struct LockedSurface {
    surface: WlSurface,
    lock_surface: ExtSessionLockSurfaceV1,
    input: usize,
    _buffer: WlBuffer,
}

fn top_bar_anchor() -> zwlr_layer_surface_v1::Anchor {
    use zwlr_layer_surface_v1::Anchor;
    Anchor::Top | Anchor::Left | Anchor::Right
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let entry = state.layers.iter_mut().find(|entry| entry.layer == *layer);
        let Some(entry) = entry else {
            return;
        };
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                entry.size = (width, height);
                entry.configures += 1;
            }
            zwlr_layer_surface_v1::Event::Closed => entry.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_session_lock_v1::Event::Locked => state.lock_events.push(LockEvent::Locked),
            ext_session_lock_v1::Event::Finished => state.lock_events.push(LockEvent::Finished),
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockSurfaceV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        surface: &ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            surface.ack_configure(serial);
            state.lock_size = Some((width, height));
        }
    }
}

impl Dispatch<ZxdgToplevelDecorationV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_toplevel_decoration_v1::Event::Configure {
            mode: WEnum::Value(mode),
        } = event
        {
            state.decoration_modes.push(mode);
        }
    }
}

impl Dispatch<WlDataDevice, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => state.offers.push(id),
            wl_data_device::Event::Selection { id } => state.selection = id,
            wl_data_device::Event::Enter {
                x, y, id, serial, ..
            } => {
                if let Some(offer) = id {
                    offer.accept(serial, Some("text/plain".into()));
                    offer.set_actions(
                        wayland_client::protocol::wl_data_device_manager::DndAction::Copy,
                        wayland_client::protocol::wl_data_device_manager::DndAction::Copy,
                    );
                    state.selection = Some(offer);
                }
                state.dnd.push(DndEvent::Enter(x, y));
            }
            wl_data_device::Event::Motion { x, y, .. } => state.dnd.push(DndEvent::Motion(x, y)),
            wl_data_device::Event::Leave => state.dnd.push(DndEvent::Leave),
            wl_data_device::Event::Drop => state.dnd.push(DndEvent::Drop),
            _ => {}
        }
    }

    event_created_child!(ClientState, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<ZwpPrimarySelectionDeviceV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwpPrimarySelectionDeviceV1,
        event: zwp_primary_selection_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_primary_selection_device_v1::Event::DataOffer { offer } => {
                state.primary_offers.push(offer);
            }
            zwp_primary_selection_device_v1::Event::Selection { id } => state.primary = id,
            _ => {}
        }
    }

    event_created_child!(ClientState, ZwpPrimarySelectionDeviceV1, [
        zwp_primary_selection_device_v1::EVT_DATA_OFFER_OPCODE => (ZwpPrimarySelectionOfferV1, ()),
    ]);
}

/// Answers every request for data with `CLIPBOARD_TEXT`.
impl Dispatch<WlDataSource, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { fd, .. } => {
                fs::File::from(fd).write_all(CLIPBOARD_TEXT).unwrap();
            }
            wl_data_source::Event::Target { .. } => state.source_events.push(SourceEvent::Target),
            wl_data_source::Event::DndDropPerformed => {
                state.source_events.push(SourceEvent::Dropped);
            }
            wl_data_source::Event::DndFinished => state.source_events.push(SourceEvent::Finished),
            wl_data_source::Event::Cancelled => state.source_events.push(SourceEvent::Cancelled),
            _ => {}
        }
    }
}

impl Dispatch<ZwpPrimarySelectionSourceV1, ()> for ClientState {
    fn event(
        _: &mut Self,
        _: &ZwpPrimarySelectionSourceV1,
        event: zwp_primary_selection_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_primary_selection_source_v1::Event::Send { fd, .. } = event {
            fs::File::from(fd).write_all(CLIPBOARD_TEXT).unwrap();
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.preferred_scale = Some(scale);
        }
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => state.idle.push(true),
            ext_idle_notification_v1::Event::Resumed => state.idle.push(false),
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_buffer_params_v1::Event::Created { .. } => state.dmabuf = Some(Ok(())),
            zwp_linux_buffer_params_v1::Event::Failed => state.dmabuf = Some(Err(())),
            _ => {}
        }
    }

    event_created_child!(ClientState, ZwpLinuxBufferParamsV1, [
        zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (WlBuffer, ()),
    ]);
}

impl Dispatch<WlOutput, ()> for ClientState {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_output_v1::Event::LogicalSize { width, height } = event {
            state.logical_size = Some((width, height));
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
                serial,
                state: WEnum::Value(key_state),
                ..
            } => {
                state.key_serial = serial;
                Input::Key(key, key_state)
            }
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
                serial,
                surface_x,
                surface_y,
            } => {
                state.enter_serial = serial;
                Input::PointerEnter(state.window_index(&surface), surface_x, surface_y)
            }
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
        match event {
            xdg_popup::Event::Configure { x, y, .. } => state.popup_position = Some((x, y)),
            xdg_popup::Event::PopupDone => state.popups_done += 1,
            _ => {}
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
delegate_noop!(ClientState: ignore ZwlrLayerShellV1);
delegate_noop!(ClientState: ignore ExtSessionLockManagerV1);
delegate_noop!(ClientState: ignore ZxdgDecorationManagerV1);
delegate_noop!(ClientState: ignore WlDataDeviceManager);
delegate_noop!(ClientState: ignore WlDataOffer);
delegate_noop!(ClientState: ignore ZwpPrimarySelectionDeviceManagerV1);
delegate_noop!(ClientState: ignore ZwpPrimarySelectionOfferV1);
delegate_noop!(ClientState: ignore WpCursorShapeManagerV1);
delegate_noop!(ClientState: ignore WpCursorShapeDeviceV1);
delegate_noop!(ClientState: ignore WpViewporter);
delegate_noop!(ClientState: ignore WpViewport);
delegate_noop!(ClientState: ignore WpFractionalScaleManagerV1);
delegate_noop!(ClientState: ignore ZwpIdleInhibitManagerV1);
delegate_noop!(ClientState: ignore ZwpIdleInhibitorV1);
delegate_noop!(ClientState: ignore ExtIdleNotifierV1);
delegate_noop!(ClientState: ignore ZwpLinuxDmabufV1);
delegate_noop!(ClientState: ignore ZxdgOutputManagerV1);
delegate_noop!(ClientState: ignore WlRegion);
delegate_noop!(ClientState: ignore ZwpPointerConstraintsV1);
delegate_noop!(ClientState: ignore ZwpRelativePointerManagerV1);
delegate_noop!(ClientState: ignore WpPresentation);
delegate_noop!(ClientState: ignore XdgActivationV1);

impl Dispatch<ZwpLockedPointerV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => state.constraint_events.push(true),
            zwp_locked_pointer_v1::Event::Unlocked => state.constraint_events.push(false),
            _ => {}
        }
    }
}

impl Dispatch<ZwpConfinedPointerV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwpConfinedPointerV1,
        event: zwp_confined_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_confined_pointer_v1::Event::Confined => state.constraint_events.push(true),
            zwp_confined_pointer_v1::Event::Unconfined => state.constraint_events.push(false),
            _ => {}
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
            ..
        } = event
        {
            state.relative.push((dx, dy, dx_unaccel, dy_unaccel));
        }
    }
}

impl Dispatch<WpPresentationFeedback, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented { .. } => state.presented.push(true),
            wp_presentation_feedback::Event::Discarded => state.presented.push(false),
            _ => {}
        }
    }
}

impl Dispatch<XdgActivationTokenV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.activation_token = Some(token);
        }
    }
}

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
        "zwlr_layer_shell_v1",
        "zxdg_decoration_manager_v1",
        "zxdg_output_manager_v1",
        "wl_data_device_manager",
        "zwp_primary_selection_device_manager_v1",
        "wp_cursor_shape_manager_v1",
        "wp_viewporter",
        "wp_fractional_scale_manager_v1",
        "ext_session_lock_manager_v1",
        "zwp_idle_inhibit_manager_v1",
        "ext_idle_notifier_v1",
        "zwp_pointer_constraints_v1",
        "zwp_relative_pointer_manager_v1",
        "wp_presentation",
        "xdg_activation_v1",
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
        state.output_resized(&output);
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
    // The loop may end right after the state changed, before the configure got here.
    survivor.sync();
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

// Control socket. These go through the real `wirbel` binary wherever a command line is involved.

impl Compositor {
    fn wayland_display(&self) -> &std::ffi::OsStr {
        self.socket.file_name().unwrap()
    }

    fn ipc_path(&self) -> PathBuf {
        wirbel::ipc::socket_path(self.wayland_display()).unwrap()
    }

    fn cli(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wirbel"));
        command
            .args(args)
            .env("WAYLAND_DISPLAY", self.wayland_display());
        command
    }

    /// Runs a command to its end and returns its standard output.
    fn run(&self, args: &[&str]) -> String {
        let output = self.cli(args).output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{args:?}: {stderr}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// Runs a command that has to fail and returns its error message.
    fn run_failing(&self, args: &[&str]) -> String {
        let output = self.cli(args).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        String::from_utf8(output.stderr).unwrap()
    }

    fn pointer(&self) -> (f64, f64) {
        self.with(|state| {
            let location = state.seat.get_pointer().unwrap().current_location();
            (location.x, location.y)
        })
    }

    fn raw_ipc(&self) -> RawIpc {
        let stream = UnixStream::connect(self.ipc_path()).unwrap();
        stream.set_read_timeout(Some(ANSWER)).unwrap();
        RawIpc {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
        }
    }
}

/// How long a test waits for a line before it counts as missing.
const ANSWER: Duration = Duration::from_secs(5);

struct RawIpc {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl RawIpc {
    fn send(&mut self, text: &str) {
        self.stream.write_all(text.as_bytes()).unwrap();
    }

    fn line(&mut self) -> String {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        line.trim_end().to_string()
    }

    fn request(&mut self, text: &str) -> String {
        self.send(text);
        self.send("\n");
        self.line()
    }
}

/// A running `wirbel pos` with its output on a pipe.
struct Watcher {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Watcher {
    fn start(compositor: &Compositor, args: &[&str]) -> Self {
        let mut child = compositor
            .cli(&[&["pos"], args].concat())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in stdout.lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }

    fn next(&self) -> String {
        self.lines.recv_timeout(ANSWER).expect("no line from pos")
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn pos_prints_the_exact_position_and_output_once() {
    let compositor = Compositor::start();
    compositor.pointer_at(123.25, 45.0625);
    assert_eq!(compositor.run(&["pos", "--once"]), "123.25 45.0625 test\n");
    assert_eq!(
        compositor.run(&["pos", "--once", "--json"]),
        "{\"x\":123.25,\"y\":45.0625,\"output\":\"test\"}\n"
    );
}

#[test]
fn pos_shows_a_change_at_once_and_only_a_change() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    compositor.pointer_at(10.0, 20.0);
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "10.0 20.0 test");

    compositor.pointer_at(11.5, 20.0);
    assert_eq!(watcher.next(), "11.5 20.0 test");
    // Things that re-evaluate the pointer without moving it print nothing.
    compositor.pointer_at(11.5, 20.0);
    client.open_window(GREEN);
    client.close(0);
    compositor.with(Wirbel::refresh_pointer_focus);
    compositor.run(&["move", "11.5", "20"]);
    compositor.run(&["move", "--rel", "0", "0"]);
    compositor.pointer_at(12.0, 20.0);
    assert_eq!(watcher.next(), "12.0 20.0 test");
}

#[test]
fn pos_follows_every_way_the_pointer_can_move() {
    let (compositor, mut client, a, _) = mango_like();
    let second = compositor.add_output("second", (WIDTH, 0));
    client.sync();
    compositor.pointer_at(750.0, 300.0);
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "750.0 300.0 test");

    // A real device.
    compositor.pointer_at(751.0, 300.0);
    assert_eq!(watcher.next(), "751.0 300.0 test");
    // `wirbel move`, absolute and relative, also from another connection.
    compositor.run(&["move", "1500", "300.5"]);
    assert_eq!(watcher.next(), "1500.0 300.5 second");
    compositor.run(&["move", "--rel", "-750", "-0.5"]);
    assert_eq!(watcher.next(), "750.0 300.0 test");
    // A warp after a change of keyboard focus.
    compositor.chord(ALT, KEY_LEFT);
    client.sync();
    assert!(client.activated(a));
    assert_eq!(watcher.next(), "254.0 300.0 test");
    // A warp to an empty output.
    compositor.chord(&[KEY_LEFTALT, KEY_LEFTSHIFT], KEY_RIGHT);
    assert_eq!(watcher.next(), "1500.0 300.0 second");
    // The output under the pointer going away moves it, possibly in two steps.
    compositor.with(move |state| state.remove_output(&second));
    let mut line = watcher.next();
    while line != "500.0 300.0 test" {
        assert!(line.ends_with(" test"), "{line}");
        line = watcher.next();
    }
}

#[test]
fn pos_notices_outputs_changing_under_a_resting_pointer() {
    let compositor = Compositor::start();
    compositor.pointer_at(999.0, 300.0);
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "999.0 300.0 test");

    // Resizing the output away from under the pointer drags it along.
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let mode = Mode {
            size: (800, 400).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        state.output_resized(&output);
    });
    assert_eq!(watcher.next(), "799.0 300.0 test");

    // The position stays, but it is on no output, and then on a new one.
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        state.remove_output(&output);
    });
    assert_eq!(watcher.next(), "799.0 300.0 -");
    compositor.add_output("new", (0, 0));
    assert_eq!(watcher.next(), "799.0 300.0 new");
}

#[test]
fn pos_as_json_streams_one_object_per_line() {
    let compositor = Compositor::start();
    compositor.pointer_at(1.0, 2.0);
    let watcher = Watcher::start(&compositor, &["--json"]);
    assert_eq!(watcher.next(), r#"{"x":1.0,"y":2.0,"output":"test"}"#);
    compositor.pointer_at(1.5, 2.0);
    assert_eq!(watcher.next(), r#"{"x":1.5,"y":2.0,"output":"test"}"#);
}

#[test]
fn watchers_are_independent_of_each_other() {
    let compositor = Compositor::start();
    let first = Watcher::start(&compositor, &[]);
    first.next();
    let second = Watcher::start(&compositor, &[]);
    second.next();
    drop(first);
    compositor.pointer_at(5.0, 5.0);
    assert_eq!(second.next(), "5.0 5.0 test");
}

#[test]
fn pos_ends_quietly_when_its_reader_goes_away() {
    let compositor = Compositor::start();
    let mut child = compositor
        .cli(&["pos"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    stdout.read_line(&mut String::new()).unwrap();
    drop(stdout);
    compositor.pointer_at(5.0, 5.0);
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < ANSWER, "pos kept running");
        compositor.pointer_at(start.elapsed().as_secs_f64(), 5.0);
    }
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

#[test]
fn pos_reports_the_end_of_the_compositor() {
    let compositor = Compositor::start();
    let mut watcher = Watcher::start(&compositor, &[]);
    watcher.next();
    drop(compositor);
    assert_eq!(watcher.child.wait().unwrap().code(), Some(1));
}

#[test]
fn commands_explain_a_missing_compositor() {
    let output = Command::new(env!("CARGO_BIN_EXE_wirbel"))
        .args(["move", "1", "2"])
        .env("WAYLAND_DISPLAY", "wayland-nobody")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("wirbel move: no wirbel on wayland-nobody"),
        "{stderr}"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_wirbel"))
        .args(["pos", "--once"])
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("WAYLAND_DISPLAY"));
}

#[test]
fn bad_command_lines_are_refused() {
    let compositor = Compositor::start();
    compositor.pointer_at(7.0, 8.0);
    for args in [
        &["move"][..],
        &["move", "1"],
        &["move", "1", "2", "3"],
        &["move", "a", "2"],
        &["move", "--rel", "1"],
        &["move", "1", "--rel", "2"],
        &["move", "1", "2", "--rel"],
        &["pos", "--nope"],
        &["pos", "extra"],
    ] {
        assert!(compositor.run_failing(args).contains("usage:"), "{args:?}");
    }
    for args in [&["move", "NaN", "2"][..], &["move", "--rel", "1", "inf"]] {
        assert!(compositor.run_failing(args).contains("finite"), "{args:?}");
    }
    assert_eq!(compositor.pointer(), (7.0, 8.0));
}

#[test]
fn move_reaches_clients_as_ordinary_motion() {
    let (compositor, mut client, a, b) = two_windows();
    let rect_a = compositor.frames()[0];
    let rect_b = compositor.frames()[1];
    compositor.run(&["move", "100.5", "200.25"]);
    client.sync();
    assert_eq!(compositor.pointer(), (100.5, 200.25));
    let (x, y) = (100.5 - f64::from(rect_a.x), 200.25 - f64::from(rect_a.y));
    assert_eq!(client.take_input(), [Input::PointerEnter(a, x, y)]);

    compositor.run(&["move", "--rel", "1", "0"]);
    client.sync();
    assert_eq!(client.take_input(), [Input::PointerMotion(x + 1.0, y)]);

    compositor.run(&["move", "800", "200"]);
    client.sync();
    let (x, y) = (800.0 - f64::from(rect_b.x), 200.0 - f64::from(rect_b.y));
    assert_eq!(
        client.take_input(),
        [Input::PointerLeave(a), Input::PointerEnter(b, x, y)]
    );

    // And a click lands where the pointer went.
    compositor.click();
    client.sync();
    assert!(
        client
            .take_input()
            .contains(&Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed))
    );
}

#[test]
fn move_focuses_like_a_mouse_under_sloppy_focus() {
    let (compositor, mut client, a, b) = mango_like();
    compositor.pointer_at(750.0, 300.0);
    client.sync();
    assert!(client.activated(b));
    compositor.run(&["move", "100", "300"]);
    client.sync();
    assert!(client.activated(a));
    assert!(!client.activated(b));
}

#[test]
fn move_is_clamped_to_the_output() {
    let compositor = Compositor::start();
    for (target, expected) in [
        (["move", "-5", "-5"], (0.0, 0.0)),
        (["move", "1000", "600"], (999.0, 599.0)),
        (["move", "1e300", "100.5"], (999.0, 100.5)),
        (["move", "100.5", "-1e300"], (100.5, 0.0)),
        (["move", "999.75", "599.5"], (999.75, 599.5)),
    ] {
        compositor.run(&target);
        assert_eq!(compositor.pointer(), expected, "{target:?}");
    }

    compositor.run(&["move", "10", "10"]);
    compositor.run(&["move", "--rel", "-100", "-100"]);
    assert_eq!(compositor.pointer(), (0.0, 0.0));
    compositor.run(&["move", "--rel", "5000", "50"]);
    assert_eq!(compositor.pointer(), (999.0, 50.0));
}

#[test]
fn move_crosses_outputs_and_stays_on_the_visible_area() {
    // Two outputs of the same size, the second one lower: below the first is nothing to be on.
    let compositor = Compositor::start();
    compositor.add_output("lower", (WIDTH, 300));

    compositor.run(&["move", "1500", "500"]);
    assert_eq!(compositor.pointer(), (1500.0, 500.0));
    assert_eq!(compositor.with(|state| state.active_monitor), 1);
    assert_eq!(compositor.run(&["pos", "--once"]), "1500.0 500.0 lower\n");

    // Into the corners that belong to no output.
    compositor.run(&["move", "1500", "100"]);
    assert_eq!(compositor.pointer(), (1500.0, 300.0));
    compositor.run(&["move", "500", "800"]);
    assert_eq!(compositor.pointer(), (500.0, 599.0));
    assert_eq!(compositor.with(|state| state.active_monitor), 0);
    // Relative across the seam, and past the far end.
    compositor.run(&["move", "999", "400"]);
    compositor.run(&["move", "--rel", "1.5", "0"]);
    assert_eq!(compositor.pointer(), (1000.5, 400.0));
    assert_eq!(compositor.with(|state| state.active_monitor), 1);
    compositor.run(&["move", "--rel", "5000", "0"]);
    assert_eq!(compositor.pointer(), (1999.0, 400.0));
}

#[test]
fn device_motion_is_clamped_like_move() {
    let compositor = Compositor::start();
    compositor.pointer_at(-50.0, 2000.0);
    assert_eq!(compositor.pointer(), (0.0, 599.0));
}

#[test]
fn move_during_a_drag_carries_the_window_along() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    compositor.pointer_at(400.0, 300.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "400.0 300.0 test");
    client.sync();
    client.take_input();

    compositor.run(&["move", "500", "350"]);
    assert_eq!(watcher.next(), "500.0 350.0 test");
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(450, 250, 300, 200))
    );
    compositor.run(&["move", "--rel", "10", "10"]);
    assert_eq!(watcher.next(), "510.0 360.0 test");
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(460, 260, 300, 200))
    );
    // The clamp holds as well.
    compositor.run(&["move", "5000", "360"]);
    assert_eq!(watcher.next(), "999.0 360.0 test");
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(949, 260, 300, 200))
    );
    compositor.run(&["move", "510", "360"]);
    assert_eq!(watcher.next(), "510.0 360.0 test");

    // Clients stay out of it until the drag ends, there.
    client.sync();
    assert_eq!(client.take_input(), []);
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    assert!(!compositor.with(|state| state.drag.is_some()));
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(460, 260, 300, 200))
    );
    client.sync();
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(dialog, 50.0, 100.0))
    );
}

#[test]
fn move_during_a_resize_drag_resizes() {
    let (compositor, mut client, _, dialog) = floating_dialog();
    compositor.pointer_at(600.0, 350.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_RIGHT, ButtonState::Pressed);
    compositor.run(&["move", "--rel", "40", "20"]);
    client.sync();
    assert_eq!(client.size(dialog), (340, 220));
    compositor.button(BTN_RIGHT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    client.sync();
    assert!(!compositor.with(|state| state.drag.is_some()));
}

#[test]
fn move_can_drop_a_tile_on_another_output() {
    let (compositor, _second) = two_outputs();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    compositor.pointer_at(500.0, 300.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    compositor.run(&["move", "1500", "300"]);
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    assert_eq!(
        compositor.with(|state| state.monitors[1].workspace().windows().len()),
        1
    );
    assert_eq!(
        compositor.with(|state| state.monitors[0].workspace().windows().len()),
        0
    );
}

#[test]
fn move_with_a_button_held_on_a_client_stays_with_that_client() {
    let (compositor, mut client, a, b) = mango_like();
    compositor.pointer_at(100.0, 300.0);
    client.sync();
    assert!(client.activated(a));
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    client.take_input();

    let rect_a = compositor.frames()[0];
    compositor.run(&["move", "800", "300"]);
    client.sync();
    assert_eq!(compositor.pointer(), (800.0, 300.0));
    assert_eq!(
        client.take_input(),
        [Input::PointerMotion(
            800.0 - f64::from(rect_a.x),
            300.0 - f64::from(rect_a.y)
        )]
    );
    assert!(client.activated(a), "focus changed under a held button");
    assert!(!client.activated(b));

    // Letting go hands the pointer to the window it is on by now, without any further motion.
    compositor.button(BTN_LEFT, ButtonState::Released);
    client.sync();
    let input = client.take_input();
    assert!(input.contains(&Input::PointerLeave(a)), "{input:?}");
    assert!(
        input
            .iter()
            .any(|input| matches!(input, Input::PointerEnter(window, ..) if *window == b)),
        "{input:?}"
    );
}

#[test]
fn warps_still_wait_for_a_drag_to_end() {
    let (compositor, _client, _, _) = floating_dialog();
    compositor.pointer_at(400.0, 300.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    compositor.with(|state| state.warp_pointer((10.0, 10.0).into()));
    assert_eq!(compositor.pointer(), (400.0, 300.0));
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
}

#[test]
fn move_without_any_output_fails_and_leaves_the_pointer() {
    let compositor = Compositor::start();
    compositor.pointer_at(10.0, 10.0);
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        state.remove_output(&output);
    });
    assert!(
        compositor
            .run_failing(&["move", "1", "2"])
            .contains("no output")
    );
    assert_eq!(compositor.pointer(), (10.0, 10.0));
    assert_eq!(compositor.run(&["pos", "--once"]), "10.0 10.0 -\n");
    assert_eq!(
        compositor.run(&["pos", "--once", "--json"]),
        "{\"x\":10.0,\"y\":10.0,\"output\":null}\n"
    );
}

#[test]
fn the_socket_answers_garbage_and_goes_on() {
    let compositor = Compositor::start();
    let mut ipc = compositor.raw_ipc();
    assert_eq!(ipc.request("dance"), "error unknown request");
    assert_eq!(ipc.request(""), "error unknown request");
    assert_eq!(ipc.request("move 1"), "error expected two numbers");
    assert_eq!(
        ipc.request("move nan 2"),
        "error coordinates must be finite"
    );
    assert_eq!(ipc.request("\u{fffd}\u{0}"), "error unknown request");
    assert_eq!(ipc.request("move 3 4"), "ok");
    assert_eq!(ipc.request("position"), "position 3.0 4.0 test");

    // Several requests in one write, and one spread over two.
    ipc.send("position\nmove 5 ");
    assert_eq!(ipc.line(), "position 3.0 4.0 test");
    ipc.send("6\n");
    assert_eq!(ipc.line(), "ok");
    assert_eq!(compositor.pointer(), (5.0, 6.0));
}

#[test]
fn an_endless_request_costs_the_sender_its_connection_only() {
    let compositor = Compositor::start();
    let mut flood = compositor.raw_ipc();
    flood.stream.set_nonblocking(true).unwrap();
    let chunk = [b'x'; 4096];
    let start = Instant::now();
    while flood.stream.write(&chunk).is_ok_and(|written| written > 0) {
        assert!(start.elapsed() < ANSWER, "the connection was not closed");
        compositor.with(|_| ());
    }
    assert_eq!(compositor.raw_ipc().request("move 1 2"), "ok");
}

#[test]
fn a_client_flooding_requests_without_reading_is_dropped() {
    let compositor = Compositor::start();
    let mut flood = compositor.raw_ipc();
    flood.stream.set_nonblocking(true).unwrap();
    let start = Instant::now();
    while flood
        .stream
        .write(b"position\n")
        .is_ok_and(|written| written > 0)
    {
        assert!(
            start.elapsed() < ANSWER * 6,
            "the connection was not closed"
        );
    }
    assert_eq!(
        compositor.raw_ipc().request("position"),
        "position 0.0 0.0 test"
    );
    let open = compositor.with(|state| state.ipc.client_count());
    assert!(open <= 2, "{open} clients still open");
}

#[test]
fn a_watcher_that_stops_reading_gets_the_latest_position_later() {
    let compositor = Compositor::start();
    let mut watcher = compositor.raw_ipc();
    assert_eq!(watcher.request("watch"), "position 0.0 0.0 test");

    // Far more than the socket buffer holds. The compositor must not wait for the reader.
    let total = 300_000;
    let start = Instant::now();
    compositor.with(move |state| {
        for step in 0..total {
            let x = 1.0 + f64::from(step) * 0.001;
            state.on_pointer_motion((x, 5.0).into(), InputTime::now());
        }
        state.on_pointer_motion((77.25, 5.0).into(), InputTime::now());
    });
    assert!(start.elapsed() < Duration::from_secs(60));

    // Everything that arrives is a whole line, and the last one is where the pointer ended up,
    // although nothing moved after the reader came back.
    let mut lines = 0;
    let mut previous = 0.0;
    loop {
        let line = watcher.line();
        let position = Position::parse(&line).unwrap_or_else(|| panic!("torn line: {line:?}"));
        lines += 1;
        if position.x == 77.25 {
            break;
        }
        assert!(position.x > previous, "{line}");
        previous = position.x;
    }
    assert!(
        lines < total,
        "nothing was dropped, the buffer is too large for this test"
    );

    compositor.pointer_at(78.0, 5.0);
    assert_eq!(watcher.line(), "position 78.0 5.0 test");
}

#[test]
fn a_spawned_client_finds_the_compositor_it_runs_in() {
    let compositor = Compositor::start();
    compositor.pointer_at(21.0, 22.0);
    let path = scratch_path("spawned-pos");
    let binary = env!("CARGO_BIN_EXE_wirbel");
    let command = format!("{binary} pos --once > {path:?}.tmp; mv {path:?}.tmp {path:?}");
    compositor.with(move |state| state.spawn(&command));
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < ANSWER,
            "the spawned command never finished"
        );
        compositor.with(|_| ());
    }
    assert_eq!(fs::read_to_string(&path).unwrap(), "21.0 22.0 test\n");
    fs::remove_file(path).unwrap();
}

// Colours of test surfaces.
const BAR: u32 = 0x0012_3456;
const POPUP: u32 = 0x00ab_cdef;
const LOCK: u32 = 0x0065_4321;
const CLIPBOARD_TEXT: &[u8] = b"from the other client";

use zwlr_layer_shell_v1::Layer as Level;

fn default_config() -> Config {
    Config::default()
}

#[test]
fn an_exclusive_zone_makes_room_and_gives_it_back() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    client.sync();

    assert_eq!(client.state.layers[bar].size, (1000, 30));
    // 10 gap and 4 border below the bar, 14 at the bottom.
    assert_eq!(compositor.frames(), [Rect::new(14, 44, 972, 542)]);
    client.sync();
    assert_eq!(client.size(a), (972, 542));

    client.close_layer(bar);
    assert_eq!(compositor.frames(), [Rect::new(14, 14, 972, 572)]);
    client.sync();
    assert_eq!(client.size(a), FULL);
}

#[test]
fn exclusive_zones_of_all_edges_add_up_with_margins() {
    use zwlr_layer_surface_v1::Anchor;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_layer(Level::Top, BAR, |layer| {
        layer.set_size(20, 0);
        layer.set_anchor(Anchor::Left | Anchor::Top | Anchor::Bottom);
        layer.set_exclusive_zone(20);
        layer.set_margin(0, 0, 0, 5);
    });
    client.open_layer(Level::Bottom, BAR, |layer| {
        layer.set_size(0, 40);
        layer.set_anchor(Anchor::Left | Anchor::Right | Anchor::Bottom);
        layer.set_exclusive_zone(40);
    });
    assert_eq!(
        compositor.with(|state| state.work_area(0)),
        Some(Rect::new(25, 0, 975, 560))
    );
    assert_eq!(compositor.frames(), [Rect::new(39, 14, 947, 532)]);
}

#[test]
fn a_zone_larger_than_the_output_leaves_the_windows_alone() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_layer(Level::Top, BAR, |layer| {
        layer.set_size(0, 30);
        layer.set_anchor(top_bar_anchor());
        layer.set_exclusive_zone(5000);
    });
    assert_eq!(compositor.frames(), [Rect::new(14, 14, 972, 572)]);
}

#[test]
fn a_surface_with_exclusive_zone_minus_one_stretches_under_a_bar() {
    use zwlr_layer_surface_v1::Anchor;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_bar(30, BAR);
    let wallpaper = client.open_layer(Level::Background, GREEN, |layer| {
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_exclusive_zone(-1);
    });
    assert_eq!(client.state.layers[wallpaper].size, (1000, 600));
    assert_eq!(compositor.screenshot().at(500, 5), BAR);
    assert_eq!(compositor.screenshot().at(500, 300), GREEN);
}

#[test]
fn layers_stack_around_the_windows() {
    use zwlr_layer_surface_v1::Anchor;
    let full = Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_layer(Level::Background, GREEN, |layer| layer.set_anchor(full));
    // In the gap only something below the windows is seen.
    assert_eq!(compositor.screenshot().at(3, 3), GREEN);
    assert_eq!(compositor.screenshot().at(500, 300), RED);

    client.open_layer(Level::Bottom, BLUE, |layer| {
        layer.set_size(1000, 100);
        layer.set_anchor(Anchor::Top);
    });
    let shot = compositor.screenshot();
    assert_eq!(shot.at(3, 3), BLUE);
    assert_eq!(shot.at(500, 50), RED);

    client.open_layer(Level::Top, BAR, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
    });
    client.open_layer(Level::Overlay, POPUP, |layer| {
        layer.set_size(50, 50);
        layer.set_anchor(Anchor::Top);
    });
    let shot = compositor.screenshot();
    assert_eq!(shot.at(500, 75), BAR);
    assert_eq!(shot.at(500, 25), POPUP);
}

#[test]
fn fullscreen_covers_the_top_layer_but_not_the_overlay() {
    use zwlr_layer_surface_v1::Anchor;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    client.open_layer(Level::Overlay, POPUP, |layer| {
        layer.set_size(50, 50);
        layer.set_anchor(Anchor::Bottom);
    });
    client.sync();
    client.draw(a, RED);

    client.toplevel(a).set_fullscreen(None);
    client.sync();
    assert_eq!(client.size(a), (WIDTH, HEIGHT));
    client.draw(a, RED);
    let shot = compositor.screenshot();
    assert_eq!(shot.at(500, 5), RED, "the bar shows through");
    assert_eq!(shot.at(500, 575), POPUP, "the overlay is gone");

    compositor.pointer_at(500.0, 300.0);
    client.take_input();
    compositor.pointer_at(500.0, 5.0);
    assert_eq!(
        client.take_input().last(),
        Some(&Input::PointerMotion(500.0, 5.0))
    );

    client.toplevel(a).unset_fullscreen();
    client.sync();
    assert_eq!(compositor.screenshot().at(500, 5), BAR);
    let input = client.layer_input(bar);
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(input, 500.0, 5.0))
    );
}

#[test]
fn maximize_stays_inside_the_work_area() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    client.open_bar(30, BAR);
    client.toplevel(a).set_maximized();
    client.sync();
    assert_eq!(compositor.frames(), [Rect::new(14, 44, 972, 542)]);
}

#[test]
fn new_floating_windows_open_in_the_middle_of_the_work_area() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_bar(100, BAR);
    client.open_sized_window((200, 100), RED, |toplevel| {
        toplevel.set_min_size(200, 100);
        toplevel.set_max_size(200, 100);
    });
    assert_eq!(compositor.frames(), [Rect::new(400, 300, 200, 100)]);
}

#[test]
fn the_pointer_reaches_a_panel_and_sloppy_focus_ignores_it() {
    let compositor = Compositor::start_with("warp_cursor = 0\n");
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    let input = client.layer_input(bar);
    client.take_input();

    compositor.pointer_at(500.0, 300.0);
    client.take_input();
    compositor.run(&["move", "400.5", "10.25"]);
    assert_eq!(
        client.take_input(),
        [
            Input::PointerLeave(a),
            Input::PointerEnter(input, 400.5, 10.25)
        ]
    );
    assert_eq!(compositor.run(&["pos", "--once"]), "400.5 10.25 test\n");
    assert!(compositor.with(|state| state.window_at((400.0, 10.0).into()).is_none()));
    assert!(compositor.with(|state| state.focused.is_some()));
}

#[test]
fn a_panel_can_be_clicked_without_taking_the_focus() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    client.take_input();

    compositor.pointer_at(500.0, 300.0);
    client.take_input();
    compositor.pointer_at(500.0, 10.0);
    compositor.click();
    let input = client.take_input();
    assert!(input.contains(&Input::PointerEnter(client.layer_input(bar), 500.0, 10.0)));
    assert!(input.contains(&Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed)));
    assert!(!input.contains(&Input::KeyboardLeave(a)));
    compositor.tap_key(KEY_A);
    assert!(
        client
            .take_input()
            .contains(&Input::Key(KEY_A, wl_keyboard::KeyState::Pressed))
    );
    assert!(client.activated(a));
}

#[test]
fn a_mouse_bind_does_not_apply_to_a_panel() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.open_bar(30, BAR);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.pointer_at(500.0, 10.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    assert!(compositor.with(|state| state.drag.is_none()));
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
}

#[test]
fn an_exclusive_layer_holds_the_keyboard_until_it_goes() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    client.take_input();

    let launcher = client.open_layer(Level::Overlay, BAR, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    });
    let input = client.layer_input(launcher);
    assert_eq!(
        client.take_input(),
        [Input::KeyboardLeave(a), Input::KeyboardEnter(input)]
    );

    // Clicking a window does not take it away.
    compositor.pointer_at(100.0, 300.0);
    compositor.click();
    client.take_input();
    compositor.tap_key(KEY_A);
    assert!(
        client
            .take_input()
            .contains(&Input::Key(KEY_A, wl_keyboard::KeyState::Pressed))
    );

    client.close_layer(launcher);
    assert_eq!(client.take_input().last(), Some(&Input::KeyboardEnter(a)));
}

#[test]
fn an_exclusive_layer_gets_the_keyboard_only_with_its_first_buffer() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    client.take_input();
    let index = client.open_unmapped_layer(Level::Top, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    });
    assert_eq!(client.take_input(), []);
    client.draw_layer(index, BAR);
    assert_eq!(client.take_input().first(), Some(&Input::KeyboardLeave(a)));
}

#[test]
fn an_exclusive_layer_on_the_bottom_acts_on_demand() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    client.open_layer(Level::Bottom, BAR, |layer| {
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    });
    assert!(!client.take_input().contains(&Input::KeyboardLeave(a)));
}

#[test]
fn an_on_demand_layer_gets_the_keyboard_by_click_and_loses_it_the_same_way() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let panel = client.open_layer(Level::Top, BAR, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    });
    let input = client.layer_input(panel);
    client.take_input();

    compositor.pointer_at(500.0, 50.0);
    compositor.click();
    assert!(client.take_input().contains(&Input::KeyboardEnter(input)));

    compositor.pointer_at(100.0, 300.0);
    assert!(!client.take_input().contains(&Input::KeyboardLeave(input)));
    compositor.click();
    let events = client.take_input();
    assert!(events.contains(&Input::KeyboardLeave(input)));
    assert!(events.contains(&Input::KeyboardEnter(a)));
}

#[test]
fn sloppy_focus_leaves_the_keyboard_on_an_on_demand_layer() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let compositor = Compositor::start_with("warp_cursor = 0\n");
    let mut client = Client::connect(&compositor);
    let _a = client.open_window(RED);
    let _b = client.open_window(GREEN);
    let panel = client.open_layer(Level::Top, BAR, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    });
    let input = client.layer_input(panel);
    compositor.pointer_at(500.0, 50.0);
    compositor.click();
    compositor.pointer_at(100.0, 300.0);
    assert!(!client.take_input().contains(&Input::KeyboardLeave(input)));
}

#[test]
fn a_closing_window_leaves_the_keyboard_on_an_on_demand_layer() {
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    let panel = client.open_layer(Level::Top, BAR, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    });
    let input = client.layer_input(panel);
    compositor.pointer_at(500.0, 50.0);
    compositor.click();
    client.take_input();
    client.close(b);
    assert!(!client.take_input().contains(&Input::KeyboardLeave(input)));
    client.close_layer(panel);
    assert_eq!(client.take_input().last(), Some(&Input::KeyboardEnter(a)));
}

#[test]
fn a_null_buffer_unmaps_a_layer_surface_until_it_starts_over() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    assert_eq!(compositor.frames(), [Rect::new(14, 44, 972, 542)]);
    assert_eq!(client.state.layers[bar].configures, 1);

    let surface = client.state.layers[bar].surface.clone();
    surface.attach(None, 0, 0);
    surface.commit();
    client.sync();
    assert_eq!(compositor.frames(), [Rect::new(14, 14, 972, 572)]);
    assert_eq!(
        compositor.screenshot().at(500, 5),
        rgb(default_config().background_color)
    );

    // Unmapping forgets everything, as if the surface had just been created.
    let layer = client.state.layers[bar].layer.clone();
    layer.set_size(0, 30);
    layer.set_anchor(top_bar_anchor());
    layer.set_exclusive_zone(30);
    surface.commit();
    client.sync();
    assert_eq!(client.state.layers[bar].configures, 2);
    client.draw_layer(bar, BAR);
    assert_eq!(compositor.frames(), [Rect::new(14, 44, 972, 542)]);
    assert_eq!(compositor.screenshot().at(500, 5), BAR);
}

#[test]
fn a_panel_follows_its_output_and_goes_with_it() {
    let compositor = Compositor::start();
    let second = compositor.add_output("second", (1000, 0));
    let mut client = Client::connect(&compositor);
    client.sync();

    let shell: ZwlrLayerShellV1 = client.bind(1..=5);
    let outputs: Vec<WlOutput> = client
        .globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|global| global.interface == "wl_output")
        .map(|global| {
            client
                .globals
                .registry()
                .bind(global.name, 4, &client.handle, ())
        })
        .collect();
    client.sync();
    // Whichever of them it is, the surface must end up on exactly that output.
    let surface = client.compositor.create_surface(&client.handle, ());
    let layer = shell.get_layer_surface(
        &surface,
        Some(&outputs[1]),
        Level::Top,
        "test".into(),
        &client.handle,
        (),
    );
    layer.set_size(0, 30);
    layer.set_anchor(top_bar_anchor());
    layer.set_exclusive_zone(30);
    surface.commit();
    client.state.others.push(surface.clone());
    client.state.layers.push(LayerState {
        surface,
        layer,
        size: (0, 0),
        configures: 0,
        closed: false,
        buffer: None,
    });
    client.sync();
    client.draw_layer(0, BAR);

    let (zones, on_second) = compositor.with(|state| {
        let zones = (state.work_area(0), state.work_area(1));
        let surface = state.space.outputs().count();
        (zones, surface)
    });
    assert_eq!(on_second, 2);
    let (x, zone) = if zones.1 == Some(Rect::new(1000, 30, 1000, 570)) {
        (1500.0, 1)
    } else {
        assert_eq!(zones.0, Some(Rect::new(0, 30, 1000, 570)));
        (500.0, 0)
    };
    compositor.pointer_at(x, 10.0);
    let input = client.layer_input(0);
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(input, 500.0, 10.0))
    );
    assert_eq!(compositor.screenshot_of(zone).at(500, 10), BAR);

    let output = if zone == 1 {
        second
    } else {
        compositor.with(|state| state.monitors[0].output.clone())
    };
    compositor.with(move |state| state.remove_output(&output));
    client.sync();
    assert!(client.state.layers[0].closed);
}

#[test]
fn a_panel_grows_with_its_output() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let mode = Mode {
            size: (800, 400).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        state.output_resized(&output);
    });
    client.sync();
    assert_eq!(client.state.layers[bar].size, (800, 30));
    assert_eq!(compositor.frames(), [Rect::new(14, 44, 772, 342)]);
}

#[test]
fn a_layer_without_output_goes_to_the_active_one_and_with_no_output_closes() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        state.remove_output(&output);
    });
    let shell: ZwlrLayerShellV1 = client.bind(1..=5);
    let surface = client.compositor.create_surface(&client.handle, ());
    let layer = shell.get_layer_surface(
        &surface,
        None,
        Level::Top,
        "test".into(),
        &client.handle,
        (),
    );
    client.state.layers.push(LayerState {
        surface,
        layer,
        size: (0, 0),
        configures: 0,
        closed: false,
        buffer: None,
    });
    client.sync();
    assert!(client.state.layers[0].closed);
}

#[test]
fn a_dying_client_takes_its_panel_along() {
    let compositor = Compositor::start();
    let mut windows = Client::connect(&compositor);
    windows.open_window(RED);
    {
        let mut panel = Client::connect(&compositor);
        panel.open_bar(30, BAR);
        assert_eq!(compositor.frames(), [Rect::new(14, 44, 972, 542)]);
    }
    windows.sync();
    let start = Instant::now();
    while compositor.frames() != [Rect::new(14, 14, 972, 572)] {
        assert!(start.elapsed() < ANSWER, "the zone stays taken");
        windows.sync();
    }
}

#[test]
fn a_panel_popup_opens_above_windows_and_stays_on_the_output() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let bar = client.open_bar(30, BAR);
    let positioner = client.positioner((950, 20), (100, 50));
    positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::SlideX);
    let surface = client.compositor.create_surface(&client.handle, ());
    let xdg_surface = client.wm_base.get_xdg_surface(&surface, &client.handle, ());
    let popup = xdg_surface.get_popup(None, &positioner, &client.handle, ());
    client.state.layers[bar].layer.get_popup(&popup);
    let popup = client.finish_popup(surface, xdg_surface, popup, (100, 50), POPUP, None);

    assert_eq!(client.state.popup_position, Some((900, 20)));
    assert_eq!(compositor.screenshot().at(950, 40), POPUP);
    compositor.pointer_at(950.0, 40.0);
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(popup.input, 50.0, 20.0))
    );
}

#[test]
fn a_popup_of_a_lower_layer_opens_above_the_windows() {
    use zwlr_layer_surface_v1::Anchor;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let desktop = client.open_layer(Level::Bottom, BAR, |layer| {
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    });
    let popup = client.open_layer_popup(desktop, (400, 200), (100, 100), POPUP, None);
    assert_eq!(compositor.screenshot().at(450, 250), POPUP);
    compositor.pointer_at(450.0, 250.0);
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(popup.input, 50.0, 50.0))
    );
}

#[test]
fn unmapping_a_panel_withdraws_its_popups_and_their_grab() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let bar = client.open_bar(30, BAR);
    compositor.pointer_at(500.0, 10.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    let serial = client.state.button_serial;
    let _popup = client.open_layer_popup(bar, (100, 100), (100, 100), POPUP, Some(serial));
    compositor.button(BTN_LEFT, ButtonState::Released);
    assert!(compositor.with(|state| state.popup_grab.is_some()));

    let surface = client.state.layers[bar].surface.clone();
    surface.attach(None, 0, 0);
    surface.commit();
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    assert!(compositor.with(|state| state.popup_grab.is_none()));
    assert!(compositor.with(|state| !state.seat.get_pointer().unwrap().is_grabbed()));
}

#[test]
fn window_popups_stay_clear_of_a_panel() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    client.open_layer(Level::Top, BAR, |layer| {
        use zwlr_layer_surface_v1::Anchor;
        layer.set_size(0, 30);
        layer.set_anchor(Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_exclusive_zone(30);
    });
    client.sync();
    client.draw(a, RED);
    let positioner = client.positioner((100, 500), (100, 200));
    positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::SlideY);
    let surface = client.compositor.create_surface(&client.handle, ());
    let xdg_surface = client.wm_base.get_xdg_surface(&surface, &client.handle, ());
    let parent = client.xdg_surface(a);
    let _popup = xdg_surface.get_popup(Some(&parent), &positioner, &client.handle, ());
    surface.commit();
    client.sync();
    // The window starts at y=14, the room ends at 570.
    assert_eq!(client.state.popup_position, Some((100, 570 - 200 - 14)));
}

/// A window with a popup holding a grab, opened by a click at (100, 100).
fn window_with_grabbing_popup() -> (Compositor, Client, usize, TestPopup) {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    let serial = client.state.button_serial;
    let parent = client.xdg_surface(a);
    let popup = client.open_popup(&parent, (200, 200), (100, 100), POPUP, Some(serial));
    compositor.button(BTN_LEFT, ButtonState::Released);
    client.take_input();
    (compositor, client, a, popup)
}

#[test]
fn a_grabbing_popup_gets_the_keyboard_and_gives_it_back() {
    let (compositor, mut client, a, popup) = window_with_grabbing_popup();
    compositor.tap_key(KEY_A);
    let input = client.take_input();
    assert!(input.contains(&Input::Key(KEY_A, wl_keyboard::KeyState::Pressed)));
    assert!(compositor.with(|state| state.popup_grab.is_some()));
    let focus = compositor.with(|state| {
        let keyboard = state.seat.get_keyboard().unwrap();
        keyboard.current_focus().map(|focus| {
            use smithay::{reexports::wayland_server::Resource, wayland::seat::WaylandFocus};
            focus.wl_surface().unwrap().id().protocol_id()
        })
    });
    assert_eq!(focus, Some(popup.surface.id().protocol_id()));

    popup.destroy();
    client.sync();
    compositor.with(|_| ());
    assert!(compositor.with(|state| state.popup_grab.is_none()));
    assert!(client.take_input().contains(&Input::KeyboardEnter(a)));
}

#[test]
fn a_click_outside_ends_the_grab_and_counts_as_a_click() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let mut other = Client::connect(&compositor);
    let b = other.open_window(GREEN);
    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    let serial = client.state.button_serial;
    let parent = client.xdg_surface(a);
    let _popup = client.open_popup(&parent, (100, 100), (100, 100), POPUP, Some(serial));
    compositor.button(BTN_LEFT, ButtonState::Released);
    client.sync();
    other.take_input();

    compositor.pointer_at(RIGHT_X + 50.0, 300.0);
    // Inside the grab, the other client hears nothing.
    assert_eq!(other.take_input(), []);
    compositor.click();
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    let input = other.take_input();
    assert!(input.contains(&Input::KeyboardEnter(b)), "{input:?}");
    assert!(input.contains(&Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed)));
    assert!(compositor.with(|state| state.popup_grab.is_none()));
}

#[test]
fn a_click_inside_the_popup_keeps_it_open() {
    let (compositor, mut client, _a, popup) = window_with_grabbing_popup();
    compositor.pointer_at(250.0, 250.0);
    let input = client.take_input();
    assert!(
        input.contains(&Input::PointerEnter(popup.input, 36.0, 36.0)),
        "{input:?}"
    );
    compositor.click();
    client.sync();
    assert_eq!(client.state.popups_done, 0);
    assert!(compositor.with(|state| state.popup_grab.is_some()));
}

#[test]
fn nested_popups_hand_the_grab_down_and_back() {
    let (compositor, mut client, _a, first) = window_with_grabbing_popup();
    compositor.pointer_at(220.0, 220.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    let serial = client.state.button_serial;
    let parent = first.xdg_surface.clone();
    let second = client.open_popup(&parent, (50, 50), (50, 50), BLUE, Some(serial));
    compositor.button(BTN_LEFT, ButtonState::Released);
    client.sync();

    compositor.tap_key(KEY_A);
    client.take_input();
    second.destroy();
    client.sync();
    compositor.tap_key(KEY_A);
    assert!(
        client
            .take_input()
            .contains(&Input::KeyboardEnter(first.input))
    );
    assert_eq!(client.state.popups_done, 0);
    assert!(compositor.with(|state| state.popup_grab.is_some()));
}

#[test]
fn hiding_a_window_withdraws_its_popups_and_frees_the_seat() {
    let (compositor, mut client, _a, _popup) = window_with_grabbing_popup();
    compositor.chord(&[KEY_LEFTMETA], KEY_2);
    client.sync();
    assert_eq!(compositor.active_workspace(), 1);
    assert_eq!(client.state.popups_done, 1);
    assert!(compositor.with(|state| {
        state.popup_grab.is_none() && !state.seat.get_pointer().unwrap().is_grabbed()
    }));
    assert!(!compositor.with(|state| state.seat.get_keyboard().unwrap().is_grabbed()));
}

#[test]
fn popups_without_a_grab_are_withdrawn_along_with_their_window() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let parent = client.xdg_surface(a);
    let _popup = client.open_popup(&parent, (100, 100), (100, 100), POPUP, None);
    compositor.chord(&[KEY_LEFTMETA], KEY_2);
    client.sync();
    assert_eq!(client.state.popups_done, 1);
}

#[test]
fn a_bound_focus_change_ends_the_grab() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    let serial = client.state.button_serial;
    let parent = client.xdg_surface(a);
    let _popup = client.open_popup(&parent, (20, 20), (100, 100), POPUP, Some(serial));
    compositor.button(BTN_LEFT, ButtonState::Released);
    client.take_input();

    compositor.chord(&[KEY_LEFTALT], KEY_RIGHT);
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    assert!(client.take_input().contains(&Input::KeyboardEnter(b)));
}

#[test]
fn pos_and_move_keep_working_through_a_popup_grab() {
    let (compositor, mut client, _a, popup) = window_with_grabbing_popup();
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "100.0 100.0 test");

    compositor.run(&["move", "250", "250"]);
    assert_eq!(watcher.next(), "250.0 250.0 test");
    assert!(
        client
            .take_input()
            .contains(&Input::PointerEnter(popup.input, 36.0, 36.0))
    );

    // The first motion after the client took the popup away must not get lost.
    popup.destroy();
    client.sync();
    compositor.run(&["move", "300", "300"]);
    assert_eq!(watcher.next(), "300.0 300.0 test");
    assert_eq!(compositor.pointer(), (300.0, 300.0));
}

#[test]
fn a_grab_with_a_foreign_pointer_grab_running_is_refused() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_RIGHT, ButtonState::Pressed);
    assert!(compositor.with(|state| state.drag.is_some()));
    let positioner = client.positioner((20, 20), (100, 100));
    let surface = client.compositor.create_surface(&client.handle, ());
    let xdg_surface = client.wm_base.get_xdg_surface(&surface, &client.handle, ());
    let parent = client.xdg_surface(a);
    let popup = xdg_surface.get_popup(Some(&parent), &positioner, &client.handle, ());
    popup.grab(&client.seat(), 1);
    surface.commit();
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    assert!(compositor.with(|state| state.drag.is_some()));
    compositor.button(BTN_RIGHT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
}

#[test]
fn every_window_is_decorated_by_the_server_whatever_it_asks() {
    use zxdg_toplevel_decoration_v1::Mode as DecorationMode;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let manager: ZxdgDecorationManagerV1 = client.bind(1..=1);
    let handle = client.handle.clone();
    let mut decoration = None;
    let a = client.open_unmapped_window_with(|toplevel| {
        decoration = Some(manager.get_toplevel_decoration(toplevel, &handle, ()));
    });
    assert_eq!(client.state.decoration_modes, [DecorationMode::ServerSide]);
    client.draw(a, RED);

    let decoration = decoration.unwrap();
    decoration.set_mode(DecorationMode::ClientSide);
    client.sync();
    assert_eq!(
        client.state.decoration_modes.last(),
        Some(&DecorationMode::ServerSide)
    );
    decoration.unset_mode();
    client.sync();
    assert_eq!(client.state.decoration_modes.len(), 3);
    assert_eq!(
        client.state.decoration_modes.last(),
        Some(&DecorationMode::ServerSide)
    );
}

fn read_pipe(read: std::os::fd::OwnedFd) -> Vec<u8> {
    use std::io::Read;
    let mut text = Vec::new();
    fs::File::from(read).read_to_end(&mut text).unwrap();
    text
}

/// Two clients with a window each. The second has the keyboard.
fn two_clients() -> (Compositor, Client, Client) {
    let compositor = Compositor::start();
    let mut first = Client::connect(&compositor);
    first.open_window(RED);
    let mut second = Client::connect(&compositor);
    second.open_window(GREEN);
    first.sync();
    (compositor, first, second)
}

/// Runs both clients until `source` has answered a pending request. Reading blocks, so that
/// happens on another thread.
fn exchange(
    source_client: &mut Client,
    target: &mut Client,
    request: impl FnOnce(&mut Client, std::os::fd::BorrowedFd<'_>),
) -> Vec<u8> {
    let (read, write) = rustix::pipe::pipe().unwrap();
    request(target, write.as_fd());
    target.sync();
    drop(write);
    source_client.sync();
    source_client.sync();
    read_pipe(read)
}

#[test]
fn the_clipboard_travels_between_clients_with_the_keyboard() {
    let (compositor, mut first, mut second) = two_clients();
    let manager: WlDataDeviceManager = second.bind(1..=3);
    let seat = second.seat();
    let device = manager.get_data_device(&seat, &second.handle, ());
    let source = manager.create_data_source(&second.handle, ());
    source.offer("text/plain".into());
    compositor.tap_key(KEY_A);
    second.sync();
    device.set_selection(Some(&source), second.state.key_serial);
    second.sync();

    let first_manager: WlDataDeviceManager = first.bind(1..=3);
    let first_seat = first.seat();
    let _first_device = first_manager.get_data_device(&first_seat, &first.handle, ());
    first.sync();
    assert!(
        first.state.selection.is_none(),
        "offered without the keyboard"
    );

    compositor.pointer_at(100.0, 100.0);
    compositor.click();
    first.sync();
    let offer = first
        .state
        .selection
        .clone()
        .expect("no offer with the keyboard");
    let text = exchange(&mut second, &mut first, |_, fd| {
        offer.receive("text/plain".into(), fd);
    });
    assert_eq!(text, CLIPBOARD_TEXT);
}

#[test]
fn the_primary_selection_travels_too() {
    let (compositor, mut first, mut second) = two_clients();
    let manager: ZwpPrimarySelectionDeviceManagerV1 = second.bind(1..=1);
    let seat = second.seat();
    let device = manager.get_device(&seat, &second.handle, ());
    let source = manager.create_source(&second.handle, ());
    source.offer("text/plain".into());
    compositor.tap_key(KEY_A);
    second.sync();
    device.set_selection(Some(&source), second.state.key_serial);
    second.sync();

    let first_manager: ZwpPrimarySelectionDeviceManagerV1 = first.bind(1..=1);
    let first_seat = first.seat();
    let _first_device = first_manager.get_device(&first_seat, &first.handle, ());
    first.sync();
    compositor.pointer_at(100.0, 100.0);
    compositor.click();
    first.sync();
    let offer = first
        .state
        .primary
        .clone()
        .expect("no offer with the keyboard");
    let text = exchange(&mut second, &mut first, |_, fd| {
        offer.receive("text/plain".into(), fd);
    });
    assert_eq!(text, CLIPBOARD_TEXT);
}

#[test]
fn drag_and_drop_reaches_another_client_and_the_pointer_stays_visible_to_pos() {
    let (compositor, mut first, mut second) = two_clients();
    let manager: WlDataDeviceManager = first.bind(1..=3);
    let seat = first.seat();
    let device = manager.get_data_device(&seat, &first.handle, ());
    let source = manager.create_data_source(&first.handle, ());
    source.offer("text/plain".into());
    source.set_actions(wayland_client::protocol::wl_data_device_manager::DndAction::Copy);
    first.sync();

    let target_manager: WlDataDeviceManager = second.bind(1..=3);
    let target_seat = second.seat();
    let _target = target_manager.get_data_device(&target_seat, &second.handle, ());
    second.sync();

    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    first.sync();
    let surface = first.state.windows[0].surface.clone();
    let icon = first.compositor.create_surface(&first.handle, ());
    device.start_drag(
        Some(&source),
        &surface,
        Some(&icon),
        first.state.button_serial,
    );
    let _icon_buffer = first.fill(&icon, 10, 10, BLUE);
    first.sync();

    assert_eq!(compositor.screenshot().at(105, 105), BLUE);
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "100.0 100.0 test");
    compositor.run(&["move", "700", "300"]);
    assert_eq!(watcher.next(), "700.0 300.0 test");
    second.sync();
    assert_eq!(
        second.state.dnd,
        [DndEvent::Enter(700.0 - RIGHT_X, 300.0 - 14.0)]
    );
    assert_eq!(compositor.screenshot().at(705, 305), BLUE);

    compositor.button(BTN_LEFT, ButtonState::Released);
    second.sync();
    assert!(second.state.dnd.contains(&DndEvent::Drop));
    first.sync();
    assert!(first.state.source_events.contains(&SourceEvent::Dropped));
    assert!(compositor.with(|state| state.dnd_icon.is_none()));
    assert!(compositor.with(|state| !state.seat.get_pointer().unwrap().is_grabbed()));
}

#[test]
fn a_cursor_shape_reaches_the_backend() {
    use smithay::input::pointer::{CursorIcon, CursorImageStatus};
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    let manager: WpCursorShapeManagerV1 = client.bind(1..=1);
    let pointer = client._pointer.clone();
    let device = manager.get_pointer(&pointer, &client.handle, ());
    compositor.pointer_at(100.0, 100.0);
    client.sync();
    device.set_shape(
        client.state.enter_serial,
        wp_cursor_shape_device_v1::Shape::Text,
    );
    client.sync();
    let status = compositor.with(|state| {
        matches!(
            state.cursor_status,
            CursorImageStatus::Named(CursorIcon::Text)
        )
    });
    assert!(status);
}

#[test]
fn a_viewport_scales_a_small_buffer_up() {
    use zwlr_layer_surface_v1::Anchor;
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let viewporter: WpViewporter = client.bind(1..=1);
    let index = client.open_unmapped_layer(Level::Top, |layer| {
        layer.set_size(100, 100);
        layer.set_anchor(Anchor::Top);
    });
    let surface = client.state.layers[index].surface.clone();
    let viewport = viewporter.get_viewport(&surface, &client.handle, ());
    viewport.set_destination(100, 100);
    let _buffer = client.fill(&surface, 10, 10, BAR);
    let shot = compositor.screenshot();
    let background = rgb(default_config().background_color);
    assert_eq!(shot.at(500, 50), BAR);
    assert_ne!(shot.at(500, 95), background, "not stretched downwards");
    assert_ne!(shot.at(545, 50), background, "not stretched sideways");
    assert_eq!(shot.at(500, 105), background);
}

#[test]
fn the_preferred_scale_comes_from_the_output_and_follows_it() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let manager: WpFractionalScaleManagerV1 = client.bind(1..=1);
    let surface = client.state.windows[a].surface.clone();
    let _scale = manager.get_fractional_scale(&surface, &client.handle, ());
    client.sync();
    assert_eq!(client.state.preferred_scale, Some(120));

    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let scale = smithay::output::Scale::Fractional(1.5);
        output.change_current_state(None, None, Some(scale), None);
        state.output_resized(&output);
    });
    client.sync();
    assert_eq!(client.state.preferred_scale, Some(180));
}

#[test]
fn xdg_output_reports_the_logical_size() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let manager: ZxdgOutputManagerV1 = client.bind(1..=3);
    let output: WlOutput = client.bind(1..=4);
    let _xdg_output = manager.get_xdg_output(&output, &client.handle, ());
    client.sync();
    assert_eq!(client.state.logical_size, Some((WIDTH, HEIGHT)));
}

/// Starts offering dmabufs of a made-up device, with `answer` as the result of every import.
fn offer_dmabufs(compositor: &Compositor, answer: bool) {
    compositor.with(move |state| {
        use smithay::backend::allocator::{Format as DmabufFormat, Modifier};
        state.import_dmabuf = Box::new(move |_| answer);
        let format = DmabufFormat {
            code: Fourcc::Argb8888,
            modifier: Modifier::Linear,
        };
        state.offer_dmabufs(1, [format]);
    });
}

fn create_dmabuf(client: &mut Client) {
    let manager: ZwpLinuxDmabufV1 = client.bind(4..=5);
    let params = manager.create_params(&client.handle, ());
    let file = fs::File::create(scratch_path("dmabuf")).unwrap();
    file.set_len(4096).unwrap();
    let modifier = u64::from(smithay::backend::allocator::Modifier::Linear);
    params.add(
        file.as_fd(),
        0,
        0,
        64,
        (modifier >> 32) as u32,
        modifier as u32,
    );
    params.create(
        16,
        16,
        Fourcc::Argb8888 as u32,
        zwp_linux_buffer_params_v1::Flags::empty(),
    );
    client.sync();
    fs::remove_file(scratch_path("dmabuf")).ok();
}

#[test]
fn a_dmabuf_the_renderer_accepts_becomes_a_buffer() {
    let compositor = Compositor::start();
    offer_dmabufs(&compositor, true);
    let mut client = Client::connect(&compositor);
    create_dmabuf(&mut client);
    assert_eq!(client.state.dmabuf, Some(Ok(())));
}

#[test]
fn a_dmabuf_the_renderer_refuses_fails_cleanly() {
    let compositor = Compositor::start();
    offer_dmabufs(&compositor, false);
    let mut client = Client::connect(&compositor);
    create_dmabuf(&mut client);
    assert_eq!(client.state.dmabuf, Some(Err(())));
}

#[test]
fn without_a_backend_there_are_no_dmabufs_to_get() {
    let compositor = Compositor::start();
    let client = Client::connect(&compositor);
    let list = client.globals.contents().clone_list();
    assert!(
        !list
            .iter()
            .any(|global| global.interface == "zwp_linux_dmabuf_v1")
    );
}

fn bound_output(client: &mut Client) -> WlOutput {
    let output = client.bind(1..=4);
    client.sync();
    output
}

/// A window and a session locked by a second client whose lock surface fills the output.
fn locked_session() -> (Compositor, Client, Client, ExtSessionLockV1, LockedSurface) {
    let compositor = Compositor::start();
    let mut windows = Client::connect(&compositor);
    windows.open_window(RED);
    let mut locker = Client::connect(&compositor);
    let lock = locker.lock();
    let output = bound_output(&mut locker);
    let surface = locker.lock_surface(&lock, &output, LOCK);
    compositor.present();
    locker.sync();
    windows.take_input();
    (compositor, windows, locker, lock, surface)
}

#[test]
fn locked_is_only_said_once_a_blank_frame_was_presented() {
    let compositor = Compositor::start();
    let mut windows = Client::connect(&compositor);
    windows.open_window(RED);
    let mut locker = Client::connect(&compositor);
    let lock = locker.lock();
    assert_eq!(locker.state.lock_events, []);

    // The window is gone from the very next frame, before any lock surface exists.
    let background = rgb(default_config().background_color);
    assert_eq!(compositor.screenshot().at(500, 300), background);
    assert_eq!(locker.state.lock_events, []);
    compositor.present();
    locker.sync();
    assert_eq!(locker.state.lock_events, [LockEvent::Locked]);

    let output = bound_output(&mut locker);
    locker.lock_surface(&lock, &output, LOCK);
    assert_eq!(locker.state.lock_size, Some((WIDTH as u32, HEIGHT as u32)));
    assert_eq!(compositor.screenshot().at(500, 300), LOCK);
}

#[test]
fn a_locked_session_shows_no_panel_and_no_window() {
    let compositor = Compositor::start();
    let mut panel = Client::connect(&compositor);
    panel.open_bar(30, BAR);
    panel.open_window(RED);
    let mut locker = Client::connect(&compositor);
    locker.lock();
    let shot = compositor.screenshot();
    let background = rgb(default_config().background_color);
    assert_eq!(shot.at(500, 5), background);
    assert_eq!(shot.at(500, 300), background);
}

#[test]
fn only_the_lock_surface_gets_keys_and_binds_are_off() {
    let (compositor, mut windows, mut locker, _lock, surface) = locked_session();
    locker.sync();
    let input = locker.take_input();
    assert!(
        input.contains(&Input::KeyboardEnter(surface.input)),
        "{input:?}"
    );
    assert_eq!(windows.take_input(), []);

    // Super+2 switches workspaces, Super+Return starts a program, Super+M quits.
    compositor.chord(&[KEY_LEFTMETA], KEY_2);
    assert_eq!(compositor.active_workspace(), 0);
    let keys = locker.take_input();
    assert!(
        keys.contains(&Input::Key(KEY_2, wl_keyboard::KeyState::Pressed)),
        "{keys:?}"
    );
    assert_eq!(windows.take_input(), []);
}

#[test]
fn the_window_loses_the_keyboard_when_the_lock_begins() {
    let compositor = Compositor::start();
    let mut windows = Client::connect(&compositor);
    let a = windows.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    windows.take_input();
    let mut locker = Client::connect(&compositor);
    locker.lock();
    let input = windows.take_input();
    assert!(input.contains(&Input::KeyboardLeave(a)), "{input:?}");
    assert!(input.contains(&Input::PointerLeave(a)), "{input:?}");
    compositor.tap_key(KEY_A);
    compositor.pointer_at(200.0, 200.0);
    compositor.click();
    assert_eq!(windows.take_input(), []);
}

#[test]
fn move_and_pos_keep_working_but_only_the_lock_surface_sees_the_pointer() {
    let (compositor, mut windows, mut locker, _lock, surface) = locked_session();
    locker.take_input();
    let watcher = Watcher::start(&compositor, &[]);
    assert_eq!(watcher.next(), "0.0 0.0 test");

    compositor.run(&["move", "300.5", "200.25"]);
    assert_eq!(watcher.next(), "300.5 200.25 test");
    compositor.run(&["move", "--rel", "1", "1"]);
    assert_eq!(watcher.next(), "301.5 201.25 test");
    assert_eq!(compositor.run(&["pos", "--once"]), "301.5 201.25 test\n");

    let input = locker.take_input();
    let _ = surface;
    assert_eq!(
        input,
        [
            Input::PointerMotion(300.5, 200.25),
            Input::PointerMotion(301.5, 201.25)
        ]
    );
    assert_eq!(windows.take_input(), []);
}

#[test]
fn clicks_of_a_locked_session_neither_focus_nor_raise_nor_drag() {
    let (compositor, mut windows, mut locker, _lock, _surface) = locked_session();
    let a = compositor.with(|state| state.focused.clone());
    compositor.pointer_at(500.0, 300.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    assert!(compositor.with(|state| state.drag.is_none()));
    compositor.button(BTN_LEFT, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    assert!(compositor.with(move |state| state.focused == a));
    assert_eq!(windows.take_input(), []);
    let input = locker.take_input();
    assert!(input.contains(&Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed)));
}

#[test]
fn locking_takes_the_pointer_away_from_a_held_button() {
    let compositor = Compositor::start();
    let mut windows = Client::connect(&compositor);
    let a = windows.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    windows.take_input();
    let mut locker = Client::connect(&compositor);
    locker.lock();
    assert!(windows.take_input().contains(&Input::PointerLeave(a)));
    compositor.run(&["move", "200", "200"]);
    compositor.button(BTN_LEFT, ButtonState::Released);
    assert_eq!(windows.take_input(), []);
    assert_eq!(compositor.pointer(), (200.0, 200.0));
}

#[test]
fn locking_ends_grabs_and_drags() {
    let (compositor, mut client, _a, _popup) = window_with_grabbing_popup();
    let mut locker = Client::connect(&compositor);
    locker.lock();
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    assert!(compositor.with(|state| !state.seat.get_keyboard().unwrap().is_grabbed()));

    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(BTN_RIGHT, ButtonState::Pressed);
    assert!(compositor.with(|state| state.drag.is_some()));
    let mut locker = Client::connect(&compositor);
    locker.lock();
    assert!(compositor.with(|state| state.drag.is_none()));
    assert!(compositor.with(|state| !state.seat.get_pointer().unwrap().is_grabbed()));
    compositor.key(KEY_LEFTMETA, KeyState::Released);
    compositor.button(BTN_RIGHT, ButtonState::Released);
}

#[test]
fn a_grab_during_the_lock_is_refused() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    compositor.pointer_at(100.0, 100.0);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    client.sync();
    let serial = client.state.button_serial;
    let mut locker = Client::connect(&compositor);
    locker.lock();
    let parent = client.xdg_surface(a);
    let _popup = client.open_popup(&parent, (20, 20), (100, 100), POPUP, Some(serial));
    client.sync();
    assert_eq!(client.state.popups_done, 1);
    assert!(compositor.with(|state| state.popup_grab.is_none()));
}

#[test]
fn a_new_window_during_the_lock_gets_nothing() {
    let (compositor, mut windows, mut locker, _lock, surface) = locked_session();
    locker.take_input();
    let b = windows.open_window(GREEN);
    assert_eq!(compositor.screenshot().at(800, 300), LOCK);
    compositor.tap_key(KEY_A);
    assert!(!windows.take_input().contains(&Input::KeyboardEnter(b)));
    let _ = surface;
    assert!(
        locker
            .take_input()
            .contains(&Input::Key(KEY_A, wl_keyboard::KeyState::Pressed))
    );
}

#[test]
fn unlocking_brings_screen_and_input_back() {
    let (compositor, mut windows, mut locker, lock, surface) = locked_session();
    compositor.pointer_at(100.0, 100.0);
    locker.take_input();
    surface.lock_surface.destroy();
    surface.surface.destroy();
    lock.unlock_and_destroy();
    locker.sync();
    assert!(!compositor.with(|state| state.lock.is_active()));
    let input = windows.take_input();
    assert!(input.contains(&Input::KeyboardEnter(0)), "{input:?}");
    assert!(
        input.contains(&Input::PointerEnter(0, 86.0, 86.0)),
        "{input:?}"
    );
    assert_eq!(compositor.screenshot().at(100, 100), RED);
    compositor.chord(&[KEY_LEFTMETA], KEY_2);
    assert_eq!(compositor.active_workspace(), 1);
}

#[test]
fn a_dead_locker_leaves_the_session_locked_for_the_next_one() {
    let (compositor, mut windows, locker, lock, surface) = locked_session();
    drop((lock, surface, locker));
    let start = Instant::now();
    while compositor.with(|state| state.lock.surfaces().count()) != 0 {
        assert!(start.elapsed() < ANSWER, "the lock surface stays");
    }
    let background = rgb(default_config().background_color);
    assert_eq!(compositor.screenshot().at(500, 300), background);
    compositor.tap_key(KEY_A);
    compositor.pointer_at(100.0, 100.0);
    compositor.click();
    assert_eq!(windows.take_input(), []);
    assert_eq!(compositor.active_workspace(), 0);

    let mut next = Client::connect(&compositor);
    let lock = next.lock();
    assert_eq!(next.state.lock_events, [LockEvent::Locked]);
    let output = bound_output(&mut next);
    let surface = next.lock_surface(&lock, &output, LOCK);
    assert_eq!(compositor.screenshot().at(500, 300), LOCK);
    assert!(
        next.take_input()
            .contains(&Input::KeyboardEnter(surface.input))
    );

    surface.lock_surface.destroy();
    lock.unlock_and_destroy();
    next.sync();
    assert_eq!(compositor.screenshot().at(500, 300), RED);
}

#[test]
fn a_second_locker_is_turned_away_while_the_first_lives() {
    let (compositor, _windows, mut locker, _lock, _surface) = locked_session();
    let mut other = Client::connect(&compositor);
    let _second = other.lock();
    assert_eq!(other.state.lock_events, [LockEvent::Finished]);
    locker.sync();
    assert_eq!(locker.state.lock_events, [LockEvent::Locked]);
}

#[test]
fn a_second_locker_is_turned_away_before_the_first_is_confirmed() {
    let compositor = Compositor::start();
    let mut first = Client::connect(&compositor);
    let _first_lock = first.lock();
    let mut other = Client::connect(&compositor);
    let _second = other.lock();
    assert_eq!(other.state.lock_events, [LockEvent::Finished]);
    compositor.present();
    first.sync();
    assert_eq!(first.state.lock_events, [LockEvent::Locked]);
}

#[test]
fn every_output_waits_for_its_blank_frame_and_keeps_its_lock_surface() {
    let compositor = Compositor::start();
    compositor.add_output("second", (1000, 0));
    let mut locker = Client::connect(&compositor);
    let lock = locker.lock();

    let first_output = compositor.with(|state| state.monitors[0].output.clone());
    compositor.with(move |state| state.send_frame_callbacks(&first_output));
    locker.sync();
    assert_eq!(locker.state.lock_events, []);
    let second_output = compositor.with(|state| state.monitors[1].output.clone());
    compositor.with(move |state| state.send_frame_callbacks(&second_output));
    locker.sync();
    assert_eq!(locker.state.lock_events, [LockEvent::Locked]);

    let outputs: Vec<WlOutput> = locker
        .globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|global| global.interface == "wl_output")
        .map(|global| {
            locker
                .globals
                .registry()
                .bind(global.name, 4, &locker.handle, ())
        })
        .collect();
    locker.sync();
    let a = locker.lock_surface(&lock, &outputs[0], LOCK);
    let b = locker.lock_surface(&lock, &outputs[1], LOCK);
    locker.take_input();

    compositor.pointer_at(1500.0, 300.0);
    let on_b = locker.take_input();
    compositor.pointer_at(500.0, 300.0);
    let on_a = locker.take_input();
    // The surfaces are on different outputs, so each side enters a different one, and the
    // keyboard goes where the pointer goes.
    let entered = |input: &[Input]| {
        input.iter().find_map(|event| match event {
            Input::KeyboardEnter(index) => Some(*index),
            _ => None,
        })
    };
    let (first, second) = (entered(&on_b), entered(&on_a));
    assert!(first.is_some() && second.is_some(), "{on_b:?} {on_a:?}");
    assert_ne!(first, second);
    assert!([a.input, b.input].contains(&first.unwrap()));
    assert!([a.input, b.input].contains(&second.unwrap()));
}

#[test]
fn removing_the_last_waiting_output_confirms_the_lock() {
    let compositor = Compositor::start();
    let second = compositor.add_output("second", (1000, 0));
    let mut locker = Client::connect(&compositor);
    let _lock = locker.lock();
    let first = compositor.with(|state| state.monitors[0].output.clone());
    compositor.with(move |state| state.send_frame_callbacks(&first));
    locker.sync();
    assert_eq!(locker.state.lock_events, []);
    compositor.with(move |state| state.remove_output(&second));
    locker.sync();
    assert_eq!(locker.state.lock_events, [LockEvent::Locked]);
}

#[test]
fn a_lock_surface_follows_the_size_of_its_output() {
    let (compositor, _windows, mut locker, _lock, _surface) = locked_session();
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        let mode = Mode {
            size: (800, 400).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        state.output_resized(&output);
    });
    locker.sync();
    assert_eq!(locker.state.lock_size, Some((800, 400)));
}

/// A client with an idle notification of a few milliseconds.
fn idle_client(compositor: &Compositor) -> (Client, ExtIdleNotificationV1) {
    let client = Client::connect(compositor);
    let notifier: ExtIdleNotifierV1 = client.bind(1..=2);
    let seat = client.seat();
    let notification = notifier.get_idle_notification(20, &seat, &client.handle, ());
    (client, notification)
}

fn wait_for_idle(client: &mut Client, wanted: usize) {
    let start = Instant::now();
    while client.state.idle.len() < wanted {
        assert!(
            start.elapsed() < ANSWER,
            "no idle event: {:?}",
            client.state.idle
        );
        client.sync();
    }
}

#[test]
fn the_session_goes_idle_and_input_wakes_it() {
    let compositor = Compositor::start();
    let (mut client, _notification) = idle_client(&compositor);
    wait_for_idle(&mut client, 1);
    assert_eq!(client.state.idle, [true]);
    compositor.pointer_at(10.0, 10.0);
    wait_for_idle(&mut client, 2);
    assert_eq!(client.state.idle, [true, false]);
}

#[test]
fn a_visible_inhibitor_holds_off_idling_and_a_hidden_one_does_not() {
    let compositor = Compositor::start();
    let (mut client, _notification) = idle_client(&compositor);
    let a = client.open_window(RED);
    let manager: ZwpIdleInhibitManagerV1 = client.bind(1..=1);
    let surface = client.state.windows[a].surface.clone();
    let _inhibitor = manager.create_inhibitor(&surface, &client.handle, ());
    client.sync();
    compositor.with(|_| ());
    compositor.pointer_at(10.0, 10.0);
    assert!(compositor.with(|state| state.idle_notifier_state.is_inhibited()));
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(100) {
        client.sync();
    }
    assert!(
        !client.state.idle.contains(&true),
        "{:?}",
        client.state.idle
    );

    compositor.chord(&[KEY_LEFTMETA], KEY_2);
    compositor.with(|_| ());
    assert!(!compositor.with(|state| state.idle_notifier_state.is_inhibited()));
    wait_for_idle(&mut client, 1);
}

/// Taking over the screen needs a VT on stdin. This also keeps tests off the DRM backend.
#[test]
fn refuses_the_drm_backend_without_a_virtual_terminal() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_wirbel"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.contains("not a virtual terminal"), "{message}");
}

const KEY_BACKSPACE: u32 = 14;
const KEY_F3: u32 = 61;

/// Lets the compositor note every VT switch, and every emergency stop, in a list of its own.
fn record_vt_switches(compositor: &Compositor) -> Arc<Mutex<Vec<i32>>> {
    let switches = Arc::new(Mutex::new(Vec::new()));
    let recorded = switches.clone();
    compositor.with(move |state| {
        state.change_vt = Some(Box::new(move |vt| recorded.lock().unwrap().push(vt)));
    });
    switches
}

fn record_emergency(compositor: &Compositor) -> Arc<Mutex<u32>> {
    let count = Arc::new(Mutex::new(0));
    let recorded = count.clone();
    compositor.with(move |state| {
        state.emergency = Box::new(move || *recorded.lock().unwrap() += 1);
    });
    count
}

#[test]
fn ctrl_alt_f_switches_the_vt_and_no_client_sees_it() {
    let compositor = Compositor::start();
    let switches = record_vt_switches(&compositor);
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.take_input();

    compositor.chord(&[KEY_LEFTCTRL, KEY_LEFTALT], KEY_F3);
    assert_eq!(*switches.lock().unwrap(), [3]);
    let input = client.take_input();
    let f_key = |event: &Input| matches!(event, Input::Key(KEY_F3, _));
    assert!(!input.iter().any(f_key), "{input:?}");
}

#[test]
fn a_locked_session_can_leave_for_another_vt_and_stays_locked() {
    let (compositor, _windows, mut locker, _lock, _surface) = locked_session();
    let switches = record_vt_switches(&compositor);
    locker.take_input();

    compositor.chord(&[KEY_LEFTCTRL, KEY_LEFTALT], KEY_F3);
    assert_eq!(*switches.lock().unwrap(), [3]);
    let input = locker.take_input();
    let f_key = |event: &Input| matches!(event, Input::Key(KEY_F3, _));
    assert!(!input.iter().any(f_key), "{input:?}");
    assert!(compositor.with(|state| state.lock.is_active()));
    assert_eq!(compositor.screenshot().at(500, 300), LOCK);
}

#[test]
fn nested_there_is_no_vt_and_the_key_stays_with_the_client() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    client.open_window(RED);
    client.take_input();

    compositor.chord(&[KEY_LEFTCTRL, KEY_LEFTALT], KEY_F3);
    let input = client.take_input();
    let press = Input::Key(KEY_F3, wl_keyboard::KeyState::Pressed);
    assert!(input.contains(&press), "{input:?}");
}

#[test]
fn the_emergency_chord_works_with_no_bind_for_it_and_while_locked() {
    let compositor = Compositor::start();
    let count = record_emergency(&compositor);
    let chord = [KEY_LEFTCTRL, KEY_LEFTALT, KEY_LEFTSHIFT];
    compositor.chord(&chord[..2], KEY_BACKSPACE);
    assert_eq!(*count.lock().unwrap(), 0);
    compositor.chord(&chord, KEY_BACKSPACE);
    assert_eq!(*count.lock().unwrap(), 1);

    let (compositor, _windows, _locker, _lock, _surface) = locked_session();
    let count = record_emergency(&compositor);
    compositor.chord(&chord, KEY_BACKSPACE);
    assert_eq!(*count.lock().unwrap(), 1);
}

#[test]
fn a_frame_from_before_the_lock_does_not_confirm_it() {
    let compositor = Compositor::start();
    let mut locker = Client::connect(&compositor);
    locker.lock();
    // What the DRM backend does when the vblank of an older frame arrives.
    compositor.with(|state| {
        let output = state.monitors[0].output.clone();
        state.send_frames(&output);
    });
    locker.sync();
    assert_eq!(locker.state.lock_events, []);
    compositor.present();
    locker.sync();
    assert_eq!(locker.state.lock_events, [LockEvent::Locked]);
}

/// The dark pixels and the light ones around (x, y), which only a drawn cursor gives.
fn differs_from_background(shot: &Screenshot, background: u32, x: i32, y: i32) -> bool {
    (0..24).any(|offset| shot.at(x + offset / 2, y + offset) != background)
}

#[test]
fn named_cursors_are_drawn_when_nobody_else_does_it_also_while_locked() {
    let compositor = Compositor::start();
    let background = rgb(default_config().background_color);
    compositor.pointer_at(300.0, 200.0);
    let shot = compositor.screenshot();
    assert!(!differs_from_background(&shot, background, 300, 200));

    compositor.with(|state| {
        state.cursors = Some(wirbel::cursor::Cursors::load(
            Some("no-such-theme"),
            Some(24),
        ));
    });
    let shot = compositor.screenshot();
    assert!(differs_from_background(&shot, background, 300, 200));

    let mut locker = Client::connect(&compositor);
    locker.lock();
    let shot = compositor.screenshot();
    assert!(differs_from_background(&shot, background, 300, 200));
}

/// A window with the pointer inside and the keyboard, and what a game needs from the compositor.
fn window_for_game() -> (Compositor, Client, ZwpPointerConstraintsV1, usize) {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let window = client.open_window(0xff0000);
    let constraints = client.bind::<ZwpPointerConstraintsV1>(1..=1);
    compositor.pointer_at(100.0, 100.0);
    client.take_input();
    (compositor, client, constraints, window)
}

impl Compositor {
    fn pointer_delta(&self, dx: f64, dy: f64) {
        self.with(move |state| {
            let delta = (dx, dy).into();
            let raw = (dx * 2.0, dy * 2.0).into();
            state.on_pointer_delta(delta, raw, InputTime::now());
        });
    }

    fn pointer_location(&self) -> (f64, f64) {
        self.with(|state| {
            let location = state.seat.get_pointer().unwrap().current_location();
            (location.x, location.y)
        })
    }
}

#[test]
fn a_relative_pointer_gets_the_motion_as_the_mouse_made_it() {
    let (compositor, mut client, _, _) = window_for_game();
    let manager = client.bind::<ZwpRelativePointerManagerV1>(1..=1);
    let _relative = manager.get_relative_pointer(&client._pointer, &client.handle, ());
    client.sync();

    compositor.pointer_delta(3.0, -4.0);
    client.sync();
    assert_eq!(client.state.relative, [(3.0, -4.0, 6.0, -8.0)]);
    assert_eq!(compositor.pointer_location(), (103.0, 96.0));
}

#[test]
fn a_locked_pointer_stays_while_the_mouse_keeps_reporting() {
    let (compositor, mut client, constraints, window) = window_for_game();
    let manager = client.bind::<ZwpRelativePointerManagerV1>(1..=1);
    let _relative = manager.get_relative_pointer(&client._pointer, &client.handle, ());
    let surface = client.state.windows[window].surface.clone();
    let _lock = constraints.lock_pointer(
        &surface,
        &client._pointer,
        None,
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    client.sync();
    client.sync();
    assert_eq!(client.state.constraint_events, [true]);

    compositor.pointer_delta(50.0, 50.0);
    client.sync();
    assert_eq!(compositor.pointer_location(), (100.0, 100.0));
    assert_eq!(client.state.relative.len(), 1);
    assert!(
        !client
            .take_input()
            .iter()
            .any(|input| matches!(input, Input::PointerMotion(..)))
    );
}

#[test]
fn a_lock_takes_the_pointer_to_the_hint_when_it_ends() {
    let (compositor, mut client, constraints, window) = window_for_game();
    let surface = client.state.windows[window].surface.clone();
    let lock = constraints.lock_pointer(
        &surface,
        &client._pointer,
        None,
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    client.sync();
    lock.set_cursor_position_hint(30.0, 40.0);
    surface.commit();
    client.sync();
    lock.destroy();
    client.sync();
    client.sync();

    let origin = compositor.focused_rect().unwrap();
    let expected = (f64::from(origin.x) + 30.0, f64::from(origin.y) + 40.0);
    assert_eq!(compositor.pointer_location(), expected);
}

#[test]
fn a_confined_pointer_slides_along_the_edge_of_its_region() {
    let (compositor, mut client, constraints, window) = window_for_game();
    let rect = compositor.focused_rect().unwrap();
    let local = |x: f64, y: f64| (f64::from(rect.x) + x, f64::from(rect.y) + y);
    let start = local(50.0, 50.0);
    compositor.pointer_at(start.0, start.1);

    let surface = client.state.windows[window].surface.clone();
    let region = client.compositor.create_region(&client.handle, ());
    region.add(0, 0, 100, 100);
    let _confined = constraints.confine_pointer(
        &surface,
        &client._pointer,
        Some(&region),
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    surface.commit();
    client.sync();
    client.sync();
    assert_eq!(client.state.constraint_events, [true]);

    compositor.pointer_delta(10.0, 0.0);
    assert_eq!(compositor.pointer_location(), local(60.0, 50.0));
    // Past the right edge, but free to go down.
    compositor.pointer_delta(100.0, 5.0);
    assert_eq!(compositor.pointer_location(), local(60.0, 55.0));
    compositor.pointer_delta(100.0, 100.0);
    assert_eq!(compositor.pointer_location(), local(60.0, 55.0));
}

#[test]
fn a_window_without_the_keyboard_cannot_catch_the_pointer() {
    let (compositor, mut client, constraints, first) = window_for_game();
    let second = client.open_window(0x00ff00);
    client.sync();
    let rects = compositor.frames();
    let left = rects[0];
    compositor.pointer_at(f64::from(left.x + 10), f64::from(left.y + 10));
    assert_ne!(first, second);

    // The pointer is over the first window, the second one has the keyboard.
    let surface = client.state.windows[first].surface.clone();
    let _lock = constraints.lock_pointer(
        &surface,
        &client._pointer,
        None,
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    client.sync();
    client.sync();
    assert_eq!(client.state.constraint_events, []);

    compositor.click();
    client.sync();
    client.sync();
    assert_eq!(client.state.constraint_events, [true]);

    compositor.chord(&[KEY_LEFTALT], KEY_RIGHT);
    client.sync();
    client.sync();
    assert_eq!(client.state.constraint_events, [true, false]);
}

#[test]
fn moving_the_pointer_away_by_command_ends_a_lock() {
    let (compositor, mut client, constraints, window) = window_for_game();
    let surface = client.state.windows[window].surface.clone();
    let _lock = constraints.lock_pointer(
        &surface,
        &client._pointer,
        None,
        Lifetime::Persistent,
        &client.handle,
        (),
    );
    client.sync();
    client.sync();
    compositor.pointer_at(2.0, 2.0);
    client.sync();
    assert_eq!(client.state.constraint_events, [true, false]);
}

/// Asks for a token, with the serial of the latest key press if `serial` is set.
fn activation_token(client: &mut Client, serial: bool, surface: &WlSurface) -> String {
    let activation = client.bind::<XdgActivationV1>(1..=1);
    let token = activation.get_activation_token(&client.handle, ());
    if serial {
        let seat: WlSeat = client.globals.bind(&client.handle, 1..=9, ()).unwrap();
        token.set_serial(client.state.key_serial, &seat);
    }
    token.set_surface(surface);
    token.commit();
    client.sync();
    client.state.activation_token.take().unwrap()
}

#[test]
fn a_token_from_a_key_press_brings_up_the_window_on_its_workspace() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let far = client.open_window(0xff0000);
    compositor.chord(&[KEY_LEFTMETA], KEY_2);
    assert_eq!(compositor.active_workspace(), 1);
    let near = client.open_window(0x00ff00);
    client.sync();
    compositor.tap_key(KEY_A);
    client.sync();

    let surface = client.state.windows[near].surface.clone();
    let token = activation_token(&mut client, true, &surface);
    let activation = client.bind::<XdgActivationV1>(1..=1);
    activation.activate(token, &client.state.windows[far].surface);
    client.sync();
    client.sync();
    assert_eq!(compositor.active_workspace(), 0);
    assert!(client.state.windows[far].activated);
}

#[test]
fn a_window_that_only_wants_attention_gets_no_focus() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let far = client.open_window(0xff0000);
    let near = client.open_window(0x00ff00);
    client.sync();

    let surface = client.state.windows[far].surface.clone();
    let token = activation_token(&mut client, false, &surface);
    let activation = client.bind::<XdgActivationV1>(1..=1);
    activation.activate(token, &client.state.windows[far].surface);
    client.sync();
    client.sync();
    assert!(client.state.windows[near].activated);
    assert!(!client.state.windows[far].activated);
}

#[test]
fn presentation_feedback_answers_with_the_frame_that_shows_the_surface() {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let window = client.open_window(0xff0000);
    let presentation = client.bind::<WpPresentation>(1..=1);

    let surface = client.state.windows[window].surface.clone();
    let _feedback = presentation.feedback(&surface, &client.handle, ());
    surface.commit();
    client.sync();

    compositor.with(|state| {
        state.space.refresh();
        let output = state.monitors[0].output.clone();
        let mut renderer = PixmanRenderer::new().unwrap();
        let size = Size::<i32, Buffer>::from((WIDTH, HEIGHT));
        let mut image: Image<'static, 'static> =
            renderer.create_buffer(Fourcc::Argb8888, size).unwrap();
        let mut framebuffer = renderer.bind(&mut image).unwrap();
        let elements = state.scene_elements(&mut renderer, &output);
        let mut tracker = OutputDamageTracker::from_output(&output);
        let result = tracker
            .render_output(&mut renderer, &mut framebuffer, 0, &elements, [0.0; 4])
            .unwrap();
        state.update_scanout_outputs(&output, &result.states);
        let mut feedback = state.presentation_feedback(&output, &result.states);
        let refresh = Refresh::fixed(Duration::from_millis(16));
        let kind = server_feedback::Kind::Vsync;
        feedback.presented(state.clock.now(), refresh, 7, kind);
    });
    client.sync();
    assert_eq!(client.state.presented, [true]);
}

/// A compositor with a running X server and an X client of it, or `None` on a machine without
/// Xwayland, where there is nothing to test.
fn with_x_server() -> Option<(Compositor, RustConnection, u32, MutexGuard<'static, ()>)> {
    // A real session has one server. Several that come and go here trip over each other's sockets.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let turn = ONE_AT_A_TIME.lock().unwrap_or_else(|err| err.into_inner());
    let compositor = Compositor::start();
    compositor.with(Wirbel::start_xwayland);
    let display = compositor.with(|state| state.xwayland.display)?;
    wait_for(|| compositor.with(|state| state.xwm.is_some()));
    let (connection, screen) = x11rb::connect(Some(&format!(":{display}"))).unwrap();
    let root = connection.setup().roots[screen].root;
    Some((compositor, connection, root, turn))
}

/// Opens an X11 window. With `override_redirect` it is a menu, without one a normal window.
fn x_window(connection: &RustConnection, root: u32, override_redirect: bool) -> u32 {
    let window = connection.generate_id().unwrap();
    let aux = CreateWindowAux::new()
        .background_pixel(0x00ff0000)
        .override_redirect(u32::from(override_redirect))
        .event_mask(EventMask::KEY_PRESS | EventMask::STRUCTURE_NOTIFY | EventMask::FOCUS_CHANGE);
    connection
        .create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            40,
            30,
            200,
            100,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &aux,
        )
        .unwrap();
    connection.map_window(window).unwrap();
    connection.flush().unwrap();
    window
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Where a window is on the screen. Its parent is the frame of the window manager.
fn x_geometry(connection: &RustConnection, window: u32) -> (i32, i32, u32, u32) {
    let geometry = connection.get_geometry(window).unwrap().reply().unwrap();
    let on_screen = connection
        .translate_coordinates(window, geometry.root, 0, 0)
        .unwrap()
        .reply()
        .unwrap();
    (
        on_screen.dst_x.into(),
        on_screen.dst_y.into(),
        geometry.width.into(),
        geometry.height.into(),
    )
}

/// Ends the Xwayland of a display the hard way.
fn kill_x_server(display: u32) {
    let wanted = format!(":{display}");
    let mine = std::process::id().to_string();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(line) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let mut words = line.split(|byte| *byte == 0);
        let is_server = words.next().is_some_and(|word| word.ends_with(b"Xwayland"))
            && words.next() == Some(wanted.as_bytes());
        let status = fs::read_to_string(entry.path().join("status")).unwrap_or_default();
        let parent = status.lines().find_map(|line| line.strip_prefix("PPid:"));
        if is_server && parent.is_some_and(|parent| parent.trim() == mine) {
            let pid = entry.file_name().into_string().unwrap();
            Command::new("kill").args(["-9", &pid]).status().unwrap();
            return;
        }
    }
    panic!("no Xwayland on {wanted}");
}

#[test]
fn an_x11_window_is_tiled_gets_the_keyboard_and_closes() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    let window = x_window(&connection, root, false);
    wait_for(|| compositor.focused_rect().is_some());

    let rect = compositor.focused_rect().unwrap();
    wait_for(|| {
        let wanted = (rect.x, rect.y, rect.w as u32, rect.h as u32);
        x_geometry(&connection, window) == wanted
    });
    assert!(rect.w > 900, "not tiled: {rect:?}");

    compositor.tap_key(KEY_A);
    let deadline = Instant::now() + Duration::from_secs(10);
    let press = loop {
        match connection.poll_for_event().unwrap() {
            Some(Event::KeyPress(press)) => break press,
            _ => {
                assert!(Instant::now() < deadline, "no key press");
                thread::sleep(Duration::from_millis(10));
            }
        }
    };
    assert_eq!(press.detail, (KEY_A + 8) as u8);

    connection.destroy_window(window).unwrap();
    connection.flush().unwrap();
    wait_for(|| compositor.focused_rect().is_none());
    assert_eq!(compositor.frames(), []);
}

#[test]
fn x11_and_wayland_windows_share_a_workspace() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    let mut client = Client::connect(&compositor);
    client.open_window(0x00ff00);
    let window = x_window(&connection, root, false);
    wait_for(|| compositor.frames().len() == 2);
    wait_for(|| {
        let rects = compositor.frames();
        let (x, y, w, h) = x_geometry(&connection, window);
        rects
            .iter()
            .any(|r| (r.x, r.y, r.w, r.h) == (x, y, w as i32, h as i32))
    });
    // The newcomer has the keyboard, and the screen shows both.
    wait_for(|| {
        compositor.with(|state| {
            let keyboard = state.seat.get_keyboard().unwrap();
            matches!(keyboard.current_focus(), Some(KeyboardFocus::X11(_)))
        })
    });
}

#[test]
fn an_x11_menu_stays_out_of_the_tiling() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    let mut client = Client::connect(&compositor);
    let wayland = client.open_window(0x00ff00);
    let rect = compositor.focused_rect().unwrap();
    let menu = x_window(&connection, root, true);
    wait_for(|| compositor.with(|state| state.overrides.len() == 1));

    assert_eq!(compositor.frames(), [rect]);
    assert_eq!(compositor.focused_rect(), Some(rect));
    assert!(client.state.windows[wayland].activated);
    let (x, y) = compositor.with(|state| {
        let window = &state.overrides[0];
        let location = state.space.element_location(window).unwrap();
        (location.x, location.y)
    });
    assert_eq!((x, y), (40, 30));

    connection.unmap_window(menu).unwrap();
    connection.flush().unwrap();
    wait_for(|| compositor.with(|state| state.overrides.is_empty()));
}

#[test]
fn an_x11_window_can_go_fullscreen() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    let window = x_window(&connection, root, false);
    wait_for(|| compositor.focused_rect().is_some());

    let atom = |name: &[u8]| {
        connection
            .intern_atom(false, name)
            .unwrap()
            .reply()
            .unwrap()
            .atom
    };
    let (state_atom, full) = (atom(b"_NET_WM_STATE"), atom(b"_NET_WM_STATE_FULLSCREEN"));
    let event = ClientMessageEvent::new(32, window, state_atom, [1, full, 0, 1, 0]);
    connection
        .send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )
        .unwrap();
    connection.flush().unwrap();
    wait_for(|| x_geometry(&connection, window) == (0, 0, WIDTH as u32, HEIGHT as u32));
    assert_eq!(
        compositor.focused_rect(),
        Some(Rect::new(0, 0, WIDTH, HEIGHT))
    );
}

/// Games on X11 grab the pointer, and Xwayland turns that into a pointer constraint.
#[test]
fn an_x11_pointer_grab_confines_the_pointer() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    let window = x_window(&connection, root, false);
    wait_for(|| compositor.focused_rect().is_some());
    compositor.pointer_at(100.0, 100.0);

    let grab = |confine: u32| {
        connection
            .grab_pointer(
                false,
                window,
                EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                confine,
                0u32,
                0u32,
            )
            .unwrap()
            .reply()
            .unwrap()
            .status
    };
    wait_for(|| grab(window) == GrabStatus::SUCCESS);
    let constrained = || {
        compositor.with(|state| {
            let pointer = state.seat.get_pointer().unwrap();
            let Some(surface) = pointer.current_focus() else {
                return false;
            };
            with_pointer_constraint(&surface, &pointer, |constraint| {
                constraint.is_some_and(|constraint| constraint.is_active())
            })
        })
    };
    wait_for(constrained);
    assert_eq!(compositor.pointer_location(), (100.0, 100.0));

    let rect = compositor.focused_rect().unwrap();
    compositor.pointer_delta(f64::from(WIDTH) * 3.0, 0.0);
    let (x, _) = compositor.pointer_location();
    assert!(x < f64::from(rect.x + rect.w), "pointer left: {x}");

    connection.ungrab_pointer(0u32).unwrap();
    connection.flush().unwrap();
    wait_for(|| !constrained());
}

#[test]
fn a_dialog_of_x11_floats_and_may_move() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    let main = x_window(&connection, root, false);
    wait_for(|| compositor.focused_rect().is_some());

    let dialog = connection.generate_id().unwrap();
    connection
        .create_window(
            COPY_DEPTH_FROM_PARENT,
            dialog,
            root,
            0,
            0,
            300,
            200,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &CreateWindowAux::new().background_pixel(0x0000ff00),
        )
        .unwrap();
    connection
        .change_property32(
            PropMode::REPLACE,
            dialog,
            AtomEnum::WM_TRANSIENT_FOR,
            AtomEnum::WINDOW,
            &[main],
        )
        .unwrap();
    connection.map_window(dialog).unwrap();
    connection.flush().unwrap();
    wait_for(|| compositor.frames().len() == 2);

    // In the middle of the output, as wide as it was made.
    wait_for(|| {
        let (x, y, w, h) = x_geometry(&connection, dialog);
        (w, h) == (300, 200) && (x, y) == ((WIDTH - 300) / 2, (HEIGHT - 200) / 2)
    });
    connection
        .configure_window(dialog, &ConfigureWindowAux::new().x(11).y(22).width(320))
        .unwrap();
    connection.flush().unwrap();
    wait_for(|| x_geometry(&connection, dialog) == (11, 22, 320, 200));
}

#[test]
fn a_dead_x_server_takes_its_windows_along_and_comes_back() {
    let Some((compositor, connection, root, _turn)) = with_x_server() else {
        return;
    };
    x_window(&connection, root, false);
    wait_for(|| compositor.focused_rect().is_some());

    kill_x_server(compositor.with(|state| state.xwayland.display).unwrap());
    wait_for(|| compositor.frames().is_empty());
    wait_for(|| compositor.with(|state| state.xwm.is_some()));
    let after = compositor.with(|state| state.xwayland.display).unwrap();
    let (connection, screen) = x11rb::connect(Some(&format!(":{after}"))).unwrap();
    let root = connection.setup().roots[screen].root;
    x_window(&connection, root, false);
    wait_for(|| compositor.focused_rect().is_some());
}
