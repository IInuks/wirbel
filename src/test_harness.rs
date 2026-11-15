//! The headless test harness (feature `test-harness`): runs the compositor without a backend on
//! its own thread and talks to it as a real Wayland client. Input is injected where a backend
//! would deliver it, output is rendered with pixman. Used by wirbel's tests (`tests/headless.rs`)
//! and by those of programs built on wirbel: `use wirbel::test_harness::*;` brings in
//! `Compositor`, `Client`, the helpers and everything they use.

pub use std::{
    cell::RefCell,
    collections::HashMap,
    env,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub use smithay::backend::input::{Axis, AxisSource};

pub use smithay::{
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

pub use smithay::{
    reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback as server_feedback,
    wayland::{pointer_constraints::with_pointer_constraint, presentation::Refresh},
};

pub use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    event_created_child,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_callback::{self, WlCallback},
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
        wl_subcompositor::WlSubcompositor,
        wl_subsurface::WlSubsurface,
        wl_surface::{self, WlSurface},
    },
};

pub use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

pub use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};

pub use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

pub use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};

pub use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{self, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};

pub use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};

pub use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};

pub use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

pub use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_confined_pointer_v1::{self, ZwpConfinedPointerV1},
    zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
    zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
};

pub use wayland_protocols::wp::presentation_time::client::{
    wp_presentation::WpPresentation,
    wp_presentation_feedback::{self, WpPresentationFeedback},
};

pub use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1,
    zwp_primary_selection_device_v1::{self, ZwpPrimarySelectionDeviceV1},
    zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1,
    zwp_primary_selection_source_v1::{self, ZwpPrimarySelectionSourceV1},
};

pub use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
    zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
};

pub use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

pub use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::XdgActivationV1,
};

pub use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
};

pub use wayland_protocols::xdg::shell::client::{
    xdg_popup::{self, XdgPopup},
    xdg_positioner::{self, XdgPositioner},
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

pub use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

pub use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

pub use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

pub use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

pub use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

pub use crate::{
    Wirbel, config::Config, focus::KeyboardFocus, handlers::Capturer, input::Scroll, ipc::Position,
    layout::Rect, workspace::Placement,
};

pub use x11rb::{
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

pub const WIDTH: i32 = 1000;

pub const HEIGHT: i32 = 600;

pub const CONTROL_TEXT: &[u8] = b"from the clipboard manager";

pub const BTN_LEFT: u32 = 0x110;

pub const BTN_RIGHT: u32 = 0x111;

// Evdev key codes.
pub const KEY_1: u32 = 2;

pub const KEY_2: u32 = 3;

pub const KEY_3: u32 = 4;

pub const KEY_Q: u32 = 16;

pub const KEY_W: u32 = 17;

pub const KEY_Y: u32 = 21; // z on a German keyboard
pub const KEY_R: u32 = 19;

pub const KEY_F: u32 = 33;

pub const KEY_A: u32 = 30;

pub const KEY_ENTER: u32 = 28;

pub const KEY_LEFTCTRL: u32 = 29;

pub const KEY_LEFTSHIFT: u32 = 42;

pub const KEY_LEFTALT: u32 = 56;

pub const KEY_LEFTMETA: u32 = 125;

pub const KEY_UP: u32 = 103;

pub const KEY_LEFT: u32 = 105;

pub const KEY_RIGHT: u32 = 106;

pub const KEY_DOWN: u32 = 108;

pub const KEY_TAB: u32 = 15;

pub type Job = Box<dyn FnOnce(&mut Wirbel) + Send>;

pub struct Compositor {
    pub jobs: channel::Sender<Job>,
    pub socket: PathBuf,
    pub thread: Option<JoinHandle<()>>,
    // Dropped after the thread has ended, with it the socket.
    pub _slot: Slot,
}

/// Smithay finds a free socket name among only 32 (`wayland-1` to `wayland-32`), and the session
/// running the tests may hold some. More test threads than that would fail to start.
pub const MAX_COMPOSITORS: usize = 16;

pub static COMPOSITORS: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

/// One of `MAX_COMPOSITORS` places, held while a compositor lives.
pub struct Slot;

impl Slot {
    pub fn take() -> Self {
        let (count, freed) = &COMPOSITORS;
        let count = count.lock().unwrap_or_else(|err| err.into_inner());
        let mut count = freed
            .wait_while(count, |count| *count >= MAX_COMPOSITORS)
            .unwrap_or_else(|err| err.into_inner());
        *count += 1;
        Self
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let (count, freed) = &COMPOSITORS;
        *count.lock().unwrap_or_else(|err| err.into_inner()) -= 1;
        freed.notify_one();
    }
}

static SETUP: std::sync::OnceLock<fn(&mut Wirbel)> = std::sync::OnceLock::new();

/// Makes every `Compositor` run `setup` on its state right after it is made, before it has an
/// output: where a program built on wirbel adds its extensions. Only the first call counts.
pub fn set_compositor_setup(setup: fn(&mut Wirbel)) {
    let _ = SETUP.set(setup);
}

impl Compositor {
    /// The default configuration with focus on click, which is what most tests want to see.
    pub fn start() -> Self {
        Self::start_with("sloppy_focus = 0\nwarp_cursor = 0\n")
    }

    pub fn start_with(config: &str) -> Self {
        let (config, errors) = Config::parse(config);
        assert_eq!(errors, []);
        let slot = Slot::take();
        let (jobs, job_source) = channel::channel::<Job>();
        let (socket_tx, socket_rx) = mpsc::channel();

        let thread = thread::spawn(move || {
            let mut event_loop = EventLoop::try_new().unwrap();
            let display = Display::new().unwrap();
            let mut state = Wirbel::new(&mut event_loop, display, config).unwrap();
            if let Some(setup) = SETUP.get() {
                setup(&mut state);
            }

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
            _slot: slot,
        }
    }

    /// Runs `job` on the compositor thread and returns its result.
    pub fn with<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Wirbel) -> R + Send + 'static,
    ) -> R {
        let (tx, rx) = mpsc::channel();
        self.jobs
            .send(Box::new(move |state| tx.send(job(state)).unwrap()))
            .unwrap();
        rx.recv().unwrap()
    }

    /// Runs the housekeeping the event loop does after every iteration. A `with` alone does not
    /// promise one: the job channel may hand over several jobs in the same dispatch.
    pub fn housekeeping(&self) {
        self.with(Wirbel::after_dispatch);
    }

    /// Waits until the compositor has noticed that all but `remaining` clients hung up. A dropped
    /// connection may close late: a process another test spawns just then holds a copy of its
    /// socket until it has started.
    pub fn wait_for_clients(&self, remaining: usize) {
        let start = Instant::now();
        loop {
            let clients = self.with(|state| {
                let mut count = 0;
                state
                    .display_handle
                    .backend_handle()
                    .with_all_clients(|_| count += 1);
                count
            });
            if clients == remaining {
                return;
            }
            assert!(
                start.elapsed() < ANSWER,
                "{clients} clients are still there"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn pointer_at(&self, x: f64, y: f64) {
        self.with(move |state| state.on_pointer_motion((x, y).into(), InputTime::now()));
    }

    pub fn click(&self) {
        self.with(|state| {
            state.on_pointer_button(BTN_LEFT, ButtonState::Pressed, InputTime::now());
            state.on_pointer_button(BTN_LEFT, ButtonState::Released, InputTime::now());
        });
    }

    pub fn button(&self, button: u32, button_state: ButtonState) {
        self.with(move |state| state.on_pointer_button(button, button_state, InputTime::now()));
    }

    /// Presses or releases a key, then lets the idle callbacks queued so far run, as the event
    /// loop does after every batch of device input (an input filter may act in an idle after the
    /// key).
    pub fn key(&self, evdev: u32, key_state: KeyState) {
        self.with(move |state| {
            state.on_key((evdev + 8).into(), key_state, InputTime::now());
        });
        self.idles();
    }

    /// Waits until the idle callbacks queued so far have run. A `with` alone does not promise
    /// it: calloop runs idles only once the job channel is empty, and it may keep handing over
    /// jobs that come in meanwhile.
    pub fn idles(&self) {
        let (tx, rx) = mpsc::channel();
        self.with(move |state| {
            state.loop_handle.insert_idle(move |_| tx.send(()).unwrap());
        });
        rx.recv_timeout(ANSWER).expect("the idle callbacks ran");
    }

    pub fn tap_key(&self, evdev: u32) {
        self.key(evdev, KeyState::Pressed);
        self.key(evdev, KeyState::Released);
    }

    /// Taps `key` with `modifiers` held.
    pub fn chord(&self, modifiers: &[u32], key: u32) {
        for modifier in modifiers {
            self.key(*modifier, KeyState::Pressed);
        }
        self.tap_key(key);
        for modifier in modifiers.iter().rev() {
            self.key(*modifier, KeyState::Released);
        }
    }

    /// Adds a 1000x600 output at `location` and returns it.
    pub fn add_output(&self, name: &'static str, location: (i32, i32)) -> Output {
        self.with(move |state| {
            let output = test_output(name, location);
            let _global = output.create_global::<Wirbel>(&state.display_handle);
            state.add_output(&output);
            output
        })
    }

    pub fn focused_rect(&self) -> Option<Rect> {
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
    pub fn frames(&self) -> Vec<Rect> {
        self.with(|state| state.frames.iter().map(|frame| frame.inner).collect())
    }

    pub fn active_workspace(&self) -> usize {
        self.with(|state| state.monitors[state.active_monitor].active)
    }

    pub fn screenshot(&self) -> Screenshot {
        self.screenshot_of(0)
    }

    /// What a backend does for screen captures after it drew `monitor`.
    pub fn run_captures(&self, monitor: usize) {
        thread_local! {
            static CAPTURER: RefCell<Capturer<Image<'static, 'static>>> = RefCell::default();
        }
        self.with(move |state| {
            state.space.refresh();
            let output = state.monitors[monitor].output.clone();
            let mut renderer = PixmanRenderer::new().unwrap();
            CAPTURER.with_borrow_mut(|capturer| capturer.run(state, &mut renderer, &output));
        });
    }

    /// What the backend does after it presented a frame on every output.
    pub fn present(&self) {
        self.with(|state| {
            let outputs = state.monitors.iter().map(|monitor| monitor.output.clone());
            for output in outputs.collect::<Vec<_>>() {
                state.send_frame_callbacks(&output);
            }
        });
    }

    /// Renders an output with pixman into XRGB pixels.
    pub fn screenshot_of(&self, monitor: usize) -> Screenshot {
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

pub fn test_output(name: &str, location: (i32, i32)) -> Output {
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

pub struct Screenshot {
    pub pixels: Vec<u8>,
}

impl Screenshot {
    /// The pixel as 0xRRGGBB.
    pub fn at(&self, x: i32, y: i32) -> u32 {
        let offset = ((y * WIDTH + x) * 4) as usize;
        let bytes = &self.pixels[offset..offset + 4];
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
    }
}

pub fn rgb(color: [f32; 4]) -> u32 {
    let channel = |value: f32| (value * 255.0).round() as u32;
    channel(color[0]) << 16 | channel(color[1]) << 8 | channel(color[2])
}

#[derive(Debug, PartialEq)]
pub enum Input {
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
pub struct ClientState {
    pub windows: Vec<WindowState>,
    pub input: Vec<Input>,
    /// Position of the latest popup configure, relative to the parent window.
    pub popup_position: Option<(i32, i32)>,
    /// Serial of the latest button press, for interactive move and resize.
    pub button_serial: u32,
    pub closed: Vec<usize>,
    /// Rate and delay of the keyboard.
    pub repeat: Option<(i32, i32)>,
    /// Surfaces that are no windows. In input events they count from `OTHER_SURFACES` on.
    pub others: Vec<WlSurface>,
    pub layers: Vec<LayerState>,
    pub popups_done: u32,
    pub lock_events: Vec<LockEvent>,
    /// Size of the latest configure of a lock surface.
    pub lock_size: Option<(u32, u32)>,
    pub decoration_modes: Vec<zxdg_toplevel_decoration_v1::Mode>,
    pub offers: Vec<WlDataOffer>,
    pub selection: Option<WlDataOffer>,
    pub primary_offers: Vec<ZwpPrimarySelectionOfferV1>,
    pub primary: Option<ZwpPrimarySelectionOfferV1>,
    pub dnd: Vec<DndEvent>,
    pub source_events: Vec<SourceEvent>,
    pub preferred_scale: Option<u32>,
    pub idle: Vec<bool>,
    pub dmabuf: Option<Result<(), ()>>,
    pub logical_size: Option<(i32, i32)>,
    /// Serial of the latest key event, for the clipboard.
    pub key_serial: u32,
    /// Serial of the latest pointer enter, for setting the cursor.
    pub enter_serial: u32,
    /// Lock and confinement going on and off.
    pub constraint_events: Vec<bool>,
    /// Accelerated and raw motion by the relative pointer.
    pub relative: Vec<(f64, f64, f64, f64)>,
    /// Whether presentation feedback said presented, each time it answered.
    pub presented: Vec<bool>,
    /// The latest activation token.
    pub activation_token: Option<String>,
    pub copy: CopyState,
    pub foreign: Vec<ForeignToplevel>,
    pub control_offers: Vec<(ZwlrDataControlOfferV1, Vec<String>)>,
    pub control_selection: Option<ZwlrDataControlOfferV1>,
    pub control_primary: Option<ZwlrDataControlOfferV1>,
    pub control_cancelled: bool,
    /// Frame callbacks done, by window index.
    pub frames_done: HashMap<usize, u32>,
    /// `wl_surface.enter` (true) and `leave` (false) events, in order.
    pub surface_outputs: Vec<(WlSurface, bool)>,
}

impl ClientState {
    /// Whether the last of the enter and leave events of `surface` said it is on an output.
    pub fn on_output(&self, surface: &WlSurface) -> bool {
        let mut events = self.surface_outputs.iter().filter(|(s, _)| s == surface);
        events.next_back().is_some_and(|(_, entered)| *entered)
    }
}

/// What a capture frame or session said, for either of the two protocols.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CopyState {
    /// Size of the buffer to bring.
    pub size: Option<(u32, u32)>,
    pub stride: u32,
    pub formats: Vec<u32>,
    /// All buffer events are through.
    pub described: bool,
    pub damage: Vec<(i32, i32, i32, i32)>,
    pub ready: bool,
    pub failed: bool,
    pub stopped: bool,
}

#[derive(Debug)]
pub struct ForeignToplevel {
    pub handle: ZwlrForeignToplevelHandleV1,
    pub title: String,
    pub app_id: String,
    pub states: Vec<u32>,
    pub outputs: i32,
    pub parent: bool,
    pub done: u32,
    pub closed: bool,
}

/// First index of surfaces in `Input` events that are no windows.
pub const OTHER_SURFACES: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LockEvent {
    Locked,
    Finished,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DndEvent {
    Enter(f64, f64),
    Motion(f64, f64),
    Leave,
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SourceEvent {
    Target,
    Dropped,
    Finished,
    Cancelled,
}

pub struct LayerState {
    pub surface: WlSurface,
    pub layer: ZwlrLayerSurfaceV1,
    pub size: (u32, u32),
    pub configures: u32,
    pub closed: bool,
    pub buffer: Option<WlBuffer>,
}

pub struct WindowState {
    pub surface: WlSurface,
    pub xdg_surface: XdgSurface,
    pub toplevel: XdgToplevel,
    /// Size of the latest configure.
    pub size: (i32, i32),
    pub activated: bool,
    pub states: Vec<xdg_toplevel::State>,
    pub configures: u32,
    pub buffer: Option<WlBuffer>,
}

impl ClientState {
    pub fn window_index(&self, surface: &WlSurface) -> usize {
        self.find_window(surface).unwrap()
    }

    pub fn other_index(&self, surface: &WlSurface) -> Option<usize> {
        let others = self.others.iter();
        let index = others.into_iter().position(|other| other == surface)?;
        Some(OTHER_SURFACES + index)
    }

    /// `None` if the client already destroyed the surface. A leave for such a surface crosses
    /// paths with the destroy request and is not an error.
    pub fn find_window(&self, surface: &WlSurface) -> Option<usize> {
        self.windows
            .iter()
            .position(|window| window.surface == *surface)
            .or_else(|| self.other_index(surface))
    }
}

pub struct Client {
    pub connection: Connection,
    pub queue: EventQueue<ClientState>,
    pub state: ClientState,
    pub handle: QueueHandle<ClientState>,
    pub globals: GlobalList,
    pub compositor: WlCompositor,
    pub wm_base: XdgWmBase,
    pub shm: WlShm,
    pub _keyboard: WlKeyboard,
    pub _pointer: WlPointer,
}

impl Client {
    pub fn connect(compositor: &Compositor) -> Self {
        Self::connect_path(&compositor.socket)
    }

    /// A client of the compositor that listens on this Wayland socket.
    pub fn connect_path(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).unwrap();
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

    pub fn sync(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }

    /// Asks for a frame callback for a window, as a client that draws on demand does.
    pub fn request_frame(&mut self, index: usize) {
        let surface = self.state.windows[index].surface.clone();
        surface.frame(&self.handle, index);
        surface.commit();
        self.sync();
    }

    /// How many frame callbacks of a window are done.
    pub fn frames_done(&self, index: usize) -> u32 {
        self.state.frames_done.get(&index).copied().unwrap_or(0)
    }

    /// Opens a toplevel and walks it through configure up to the first buffer.
    pub fn open_window(&mut self, color: u32) -> usize {
        let index = self.open_unmapped_window();
        self.draw(index, color);
        index
    }

    /// Opens a toplevel and stops after the initial configure.
    pub fn open_unmapped_window(&mut self) -> usize {
        self.open_unmapped_window_with(|_| {})
    }

    /// Like `open_unmapped_window`, with a chance to set the window up before the first commit.
    pub fn open_unmapped_window_with(&mut self, setup: impl FnOnce(&XdgToplevel)) -> usize {
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
    pub fn open_sized_window(
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
    pub fn draw(&mut self, index: usize, color: u32) {
        self.draw_sized(index, self.state.windows[index].size, color);
    }

    pub fn draw_sized(&mut self, index: usize, (width, height): (i32, i32), color: u32) {
        let surface = self.state.windows[index].surface.clone();
        let buffer = self.fill(&surface, width, height, color);
        self.state.windows[index].buffer = Some(buffer);
    }

    pub fn fill(&mut self, surface: &WlSurface, width: i32, height: i32, color: u32) -> WlBuffer {
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
    pub fn open_edge_popup(&mut self, index: usize) -> (WlSurface, XdgPopup) {
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
    pub fn track(&mut self, surface: &WlSurface) -> usize {
        self.state.others.push(surface.clone());
        OTHER_SURFACES + self.state.others.len() - 1
    }

    pub fn bind<I: Proxy + 'static>(&self, versions: std::ops::RangeInclusive<u32>) -> I
    where
        ClientState: Dispatch<I, ()>,
    {
        self.globals.bind(&self.handle, versions, ()).unwrap()
    }

    /// Creates a layer surface and walks it through the initial configure. `setup` runs before
    /// the first commit.
    pub fn open_unmapped_layer(
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

    pub fn open_layer(
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
    pub fn open_bar(&mut self, height: u32, color: u32) -> usize {
        self.open_layer(zwlr_layer_shell_v1::Layer::Top, color, |layer| {
            layer.set_size(0, height);
            layer.set_anchor(top_bar_anchor());
            layer.set_exclusive_zone(height as i32);
        })
    }

    pub fn draw_layer(&mut self, index: usize, color: u32) {
        let layer = &self.state.layers[index];
        let surface = layer.surface.clone();
        let (w, h) = (layer.size.0 as i32, layer.size.1 as i32);
        let buffer = self.fill(&surface, w, h, color);
        self.state.layers[index].buffer = Some(buffer);
    }

    pub fn close_layer(&mut self, index: usize) {
        let layer = &self.state.layers[index];
        layer.layer.destroy();
        layer.surface.destroy();
        self.sync();
    }

    /// Input index of the surface of a layer surface.
    pub fn layer_input(&self, index: usize) -> usize {
        self.state
            .other_index(&self.state.layers[index].surface)
            .unwrap()
    }

    /// A positioner for a popup of the given size at `(x, y)` of its parent.
    pub fn positioner(&self, (x, y): (i32, i32), (w, h): (i32, i32)) -> XdgPositioner {
        let positioner = self.wm_base.create_positioner(&self.handle, ());
        positioner.set_size(w, h);
        positioner.set_anchor_rect(x, y, 1, 1);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        positioner
    }

    /// Opens and draws a popup of `parent`, grabbing with `grab` if given.
    pub fn open_popup(
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

    pub fn finish_popup(
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
    pub fn open_layer_popup(
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

    /// A buffer of plain memory for a capture, and the file it lives in.
    pub fn capture_buffer(&mut self, width: i32, height: i32) -> (WlBuffer, fs::File) {
        static NEXT_FILE: AtomicU32 = AtomicU32::new(0);
        let path = env::temp_dir().join(format!(
            "wirbel-capture-{}-{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        fs::remove_file(&path).unwrap();
        file.set_len((width * height * 4) as u64).unwrap();
        let pool = self
            .shm
            .create_pool(file.as_fd(), width * height * 4, &self.handle, ());
        let format = Format::Xrgb8888;
        let buffer = pool.create_buffer(0, width, height, width * 4, format, &self.handle, ());
        pool.destroy();
        (buffer, file)
    }

    /// Checks the buffer events of a new `wlr-screencopy` frame and returns the size they name.
    pub fn describe_frame(&mut self) -> (i32, i32) {
        self.sync();
        assert!(self.state.copy.described, "no buffer_done");
        let (width, height) = self.state.copy.size.expect("no buffer event");
        assert_eq!(self.state.copy.formats, [Format::Xrgb8888 as u32]);
        assert_eq!(self.state.copy.stride, width * 4);
        (width as i32, height as i32)
    }

    pub fn xdg_surface(&self, index: usize) -> XdgSurface {
        self.state.windows[index].xdg_surface.clone()
    }

    /// Asks for the session lock and returns the lock object.
    pub fn lock(&mut self) -> ExtSessionLockV1 {
        let manager: ExtSessionLockManagerV1 = self.bind(1..=1);
        let lock = manager.lock(&self.handle, ());
        self.sync();
        lock
    }

    /// Creates a lock surface on the first output of this client and draws it. Returns its
    /// index in input events.
    pub fn lock_surface(
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

    pub fn close(&mut self, index: usize) {
        let window = &self.state.windows[index];
        window.toplevel.destroy();
        window.xdg_surface.destroy();
        window.surface.destroy();
        self.sync();
    }

    pub fn take_input(&mut self) -> Vec<Input> {
        self.sync();
        std::mem::take(&mut self.state.input)
    }

    pub fn size(&self, index: usize) -> (i32, i32) {
        self.state.windows[index].size
    }

    pub fn activated(&self, index: usize) -> bool {
        self.state.windows[index].activated
    }

    pub fn has_state(&self, index: usize, state: xdg_toplevel::State) -> bool {
        self.state.windows[index].states.contains(&state)
    }

    pub fn toplevel(&self, index: usize) -> XdgToplevel {
        self.state.windows[index].toplevel.clone()
    }

    pub fn seat(&self) -> WlSeat {
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

impl Dispatch<WlSurface, ()> for ClientState {
    fn event(
        state: &mut Self,
        surface: &WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::Enter { .. } => state.surface_outputs.push((surface.clone(), true)),
            wl_surface::Event::Leave { .. } => state.surface_outputs.push((surface.clone(), false)),
            _ => {}
        }
    }
}

impl Dispatch<WlCallback, usize> for ClientState {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            *state.frames_done.entry(*index).or_default() += 1;
        }
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

pub struct TestPopup {
    pub surface: WlSurface,
    pub xdg_surface: XdgSurface,
    pub popup: XdgPopup,
    pub input: usize,
    pub _buffer: WlBuffer,
}

impl TestPopup {
    pub fn destroy(&self) {
        self.popup.destroy();
        self.xdg_surface.destroy();
        self.surface.destroy();
    }
}

pub struct LockedSurface {
    pub surface: WlSurface,
    pub lock_surface: ExtSessionLockSurfaceV1,
    pub input: usize,
    pub _buffer: WlBuffer,
}

pub fn top_bar_anchor() -> zwlr_layer_surface_v1::Anchor {
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

delegate_noop!(ClientState: ignore WlSubcompositor);

delegate_noop!(ClientState: ignore WlSubsurface);

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

delegate_noop!(ClientState: ignore ZwlrScreencopyManagerV1);

delegate_noop!(ClientState: ignore ExtImageCopyCaptureManagerV1);

delegate_noop!(ClientState: ignore ExtOutputImageCaptureSourceManagerV1);

delegate_noop!(ClientState: ignore ExtImageCaptureSourceV1);

delegate_noop!(ClientState: ignore ZwlrDataControlManagerV1);

impl Dispatch<ZwlrScreencopyFrameV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                state.copy.size = Some((width, height));
                state.copy.stride = stride;
                if let WEnum::Value(format) = format {
                    state.copy.formats.push(format as u32);
                }
            }
            Event::BufferDone => state.copy.described = true,
            Event::Damage {
                x,
                y,
                width,
                height,
            } => {
                let rect = (x as i32, y as i32, width as i32, height as i32);
                state.copy.damage.push(rect);
            }
            Event::Ready { .. } => state.copy.ready = true,
            Event::Failed => state.copy.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => state.copy.size = Some((width, height)),
            Event::ShmFormat {
                format: WEnum::Value(format),
            } => state.copy.formats.push(format as u32),
            Event::Done => state.copy.described = true,
            Event::Stopped => state.copy.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Damage {
                x,
                y,
                width,
                height,
            } => state.copy.damage.push((x, y, width, height)),
            Event::Ready => state.copy.ready = true,
            Event::Failed { .. } => state.copy.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            state.foreign.push(ForeignToplevel {
                handle: toplevel,
                title: String::new(),
                app_id: String::new(),
                states: Vec::new(),
                outputs: 0,
                parent: false,
                done: 0,
                closed: false,
            });
        }
    }

    event_created_child!(ClientState, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event;
        let entry = state
            .foreign
            .iter_mut()
            .find(|entry| entry.handle == *handle);
        let entry = entry.expect("event for an unknown toplevel");
        match event {
            Event::Title { title } => entry.title = title,
            Event::AppId { app_id } => entry.app_id = app_id,
            Event::OutputEnter { .. } => entry.outputs += 1,
            Event::OutputLeave { .. } => entry.outputs -= 1,
            Event::State { state } => {
                let states = state.chunks_exact(4);
                entry.states = states
                    .map(|bytes| u32::from_ne_bytes(bytes.try_into().unwrap()))
                    .collect();
            }
            Event::Parent { parent } => entry.parent = parent.is_some(),
            Event::Done => entry.done += 1,
            Event::Closed => entry.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => state.control_offers.push((id, Vec::new())),
            Event::Selection { id } => state.control_selection = id,
            Event::PrimarySelection { id } => state.control_primary = id,
            _ => {}
        }
    }

    event_created_child!(ClientState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some(entry) = state.control_offers.iter_mut().find(|(o, _)| o == offer)
        {
            entry.1.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { fd, .. } => {
                fs::File::from(fd).write_all(CONTROL_TEXT).unwrap();
            }
            zwlr_data_control_source_v1::Event::Cancelled => state.control_cancelled = true,
            _ => {}
        }
    }
}

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
pub const FULL: (i32, i32) = (972, 572);

pub const LEFT: (i32, i32) = (480, 572);

pub const RIGHT: (i32, i32) = (479, 572);

pub const RIGHT_X: f64 = 507.0;

// Stage 2: binds, workspaces, floating, fullscreen, mouse.

pub const SUPER: &[u32] = &[KEY_LEFTMETA];

pub const SUPER_SHIFT: &[u32] = &[KEY_LEFTMETA, KEY_LEFTSHIFT];

pub const ALT: &[u32] = &[KEY_LEFTALT];

pub const RED: u32 = 0x00ff_0000;

pub const GREEN: u32 = 0x0000_ff00;

pub const BLUE: u32 = 0x0000_00ff;

/// A compositor with windows `a` and `b` side by side, `b` focused.
pub fn two_windows() -> (Compositor, Client, usize, usize) {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    client.take_input();
    (compositor, client, a, b)
}

/// A scratch file path for one test.
pub fn scratch_path(name: &str) -> PathBuf {
    env::temp_dir().join(format!("wirbel-test-{}-{name}", std::process::id()))
}

pub fn is_tiled(compositor: &Compositor) -> Vec<bool> {
    compositor.with(|state| state.frames.iter().map(|frame| frame.tiled).collect())
}

/// Holds Super and drags with `button` from one point to another.
pub fn super_drag(compositor: &Compositor, button: u32, from: (f64, f64), to: (f64, f64)) {
    compositor.pointer_at(from.0, from.1);
    compositor.key(KEY_LEFTMETA, KeyState::Pressed);
    compositor.button(button, ButtonState::Pressed);
    compositor.pointer_at(to.0, to.1);
    compositor.button(button, ButtonState::Released);
    compositor.key(KEY_LEFTMETA, KeyState::Released);
}

/// A window of 300x200 floating over `main`, at 350,200.
pub fn floating_dialog() -> (Compositor, Client, usize, usize) {
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

/// The defaults: sloppy focus and warping on.
pub fn mango_like() -> (Compositor, Client, usize, usize) {
    let compositor = Compositor::start_with("");
    let mut client = Client::connect(&compositor);
    let a = client.open_window(RED);
    let b = client.open_window(GREEN);
    client.take_input();
    (compositor, client, a, b)
}

/// Two 1000x600 outputs side by side, the second one to the right.
pub fn two_outputs() -> (Compositor, Output) {
    let compositor = Compositor::start();
    let second = compositor.add_output("second", (WIDTH, 0));
    (compositor, second)
}

pub fn scroll(
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

// Control socket. These go through the real `wirbel` binary wherever a command line is involved.

static CLI_BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Makes `Compositor::cli` run this binary, such as `env!("CARGO_BIN_EXE_NAME")` of a program
/// built on wirbel. Only the first call counts.
pub fn set_cli_binary(path: impl Into<PathBuf>) {
    let _ = CLI_BINARY.set(path.into());
}

/// What `Compositor::cli` runs: the binary given to `set_cli_binary`, else `wirbel` in the
/// target directory of the test (`target/debug/deps/TEST` → `target/debug/wirbel`).
pub fn cli_binary() -> PathBuf {
    CLI_BINARY
        .get_or_init(|| {
            let test = std::env::current_exe().unwrap();
            test.parent().unwrap().parent().unwrap().join("wirbel")
        })
        .clone()
}

impl Compositor {
    pub fn wayland_display(&self) -> &std::ffi::OsStr {
        self.socket.file_name().unwrap()
    }

    pub fn ipc_path(&self) -> PathBuf {
        crate::ipc::socket_path(self.wayland_display()).unwrap()
    }

    pub fn cli(&self, args: &[&str]) -> Command {
        let mut command = Command::new(cli_binary());
        command
            .args(args)
            .env("WAYLAND_DISPLAY", self.wayland_display());
        command
    }

    /// Runs a command to its end and returns its standard output.
    pub fn run(&self, args: &[&str]) -> String {
        let output = self.cli(args).output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{args:?}: {stderr}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// Runs a command that has to fail and returns its error message.
    pub fn run_failing(&self, args: &[&str]) -> String {
        let output = self.cli(args).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        String::from_utf8(output.stderr).unwrap()
    }

    pub fn pointer(&self) -> (f64, f64) {
        self.with(|state| {
            let location = state.seat.get_pointer().unwrap().current_location();
            (location.x, location.y)
        })
    }

    pub fn raw_ipc(&self) -> RawIpc {
        let stream = UnixStream::connect(self.ipc_path()).unwrap();
        stream.set_read_timeout(Some(ANSWER)).unwrap();
        RawIpc {
            reader: BufReader::new(stream.try_clone().unwrap()),
            stream,
        }
    }
}

/// How long a test waits for a line before it counts as missing.
pub const ANSWER: Duration = Duration::from_secs(5);

pub struct RawIpc {
    pub stream: UnixStream,
    pub reader: BufReader<UnixStream>,
}

impl RawIpc {
    pub fn send(&mut self, text: &str) {
        self.stream.write_all(text.as_bytes()).unwrap();
    }

    pub fn line(&mut self) -> String {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        line.trim_end().to_string()
    }

    pub fn request(&mut self, text: &str) -> String {
        self.send(text);
        self.send("\n");
        self.line()
    }
}

/// A running `wirbel pos` or `wirbel distance` with its output on a pipe.
pub struct Watcher {
    pub child: Child,
    pub lines: mpsc::Receiver<String>,
}

impl Watcher {
    pub fn start(compositor: &Compositor, args: &[&str]) -> Self {
        Self::start_command(compositor, &[&["pos"], args].concat())
    }

    pub fn start_command(compositor: &Compositor, args: &[&str]) -> Self {
        let mut child = compositor.cli(args).stdout(Stdio::piped()).spawn().unwrap();
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

    pub fn next(&self) -> String {
        self.lines.recv_timeout(ANSWER).expect("no line from pos")
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn presses(input: &[Input]) -> usize {
    let press = Input::Button(BTN_LEFT, wl_pointer::ButtonState::Pressed);
    input.iter().filter(|event| **event == press).count()
}

// Colours of test surfaces.
pub const BAR: u32 = 0x0012_3456;

pub const POPUP: u32 = 0x00ab_cdef;

pub const LOCK: u32 = 0x0065_4321;

pub const CLIPBOARD_TEXT: &[u8] = b"from the other client";

pub use zwlr_layer_shell_v1::Layer as Level;

pub fn default_config() -> Config {
    Config::default()
}

/// A window with a popup holding a grab, opened by a click at (100, 100).
pub fn window_with_grabbing_popup() -> (Compositor, Client, usize, TestPopup) {
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

pub fn read_pipe(read: std::os::fd::OwnedFd) -> Vec<u8> {
    use std::io::Read;
    let mut text = Vec::new();
    fs::File::from(read).read_to_end(&mut text).unwrap();
    text
}

/// Two clients with a window each. The second has the keyboard.
pub fn two_clients() -> (Compositor, Client, Client) {
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
pub fn exchange(
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

/// Starts offering dmabufs of a made-up device, with `answer` as the result of every import.
pub fn offer_dmabufs(compositor: &Compositor, answer: bool) {
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

pub fn create_dmabuf(client: &mut Client) {
    let manager: ZwpLinuxDmabufV1 = client.bind(4..=5);
    let params = manager.create_params(&client.handle, ());
    // A file of its own: tests running at the same time would cut each other's to nothing.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let path = scratch_path(&format!("dmabuf-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
    let file = fs::File::create(&path).unwrap();
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
    fs::remove_file(path).ok();
}

pub fn bound_output(client: &mut Client) -> WlOutput {
    let output = client.bind(1..=4);
    client.sync();
    output
}

/// A window and a session locked by a second client whose lock surface fills the output.
pub fn locked_session() -> (Compositor, Client, Client, ExtSessionLockV1, LockedSurface) {
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

/// A client with an idle notification of a few milliseconds.
pub fn idle_client(compositor: &Compositor) -> (Client, ExtIdleNotificationV1) {
    let client = Client::connect(compositor);
    let notifier: ExtIdleNotifierV1 = client.bind(1..=2);
    let seat = client.seat();
    let notification = notifier.get_idle_notification(20, &seat, &client.handle, ());
    (client, notification)
}

pub fn wait_for_idle(client: &mut Client, wanted: usize) {
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

pub const KEY_BACKSPACE: u32 = 14;

pub const KEY_F3: u32 = 61;

/// Lets the compositor note every VT switch, and every emergency stop, in a list of its own.
pub fn record_vt_switches(compositor: &Compositor) -> Arc<Mutex<Vec<i32>>> {
    let switches = Arc::new(Mutex::new(Vec::new()));
    let recorded = switches.clone();
    compositor.with(move |state| {
        state.change_vt = Some(Box::new(move |vt| recorded.lock().unwrap().push(vt)));
    });
    switches
}

pub fn record_emergency(compositor: &Compositor) -> Arc<Mutex<u32>> {
    let count = Arc::new(Mutex::new(0));
    let recorded = count.clone();
    compositor.with(move |state| {
        state.emergency = Box::new(move || *recorded.lock().unwrap() += 1);
    });
    count
}

/// The dark pixels and the light ones around (x, y), which only a drawn cursor gives.
pub fn differs_from_background(shot: &Screenshot, background: u32, x: i32, y: i32) -> bool {
    (0..24).any(|offset| shot.at(x + offset / 2, y + offset) != background)
}

/// A window with the pointer inside and the keyboard, and what a game needs from the compositor.
pub fn window_for_game() -> (Compositor, Client, ZwpPointerConstraintsV1, usize) {
    let compositor = Compositor::start();
    let mut client = Client::connect(&compositor);
    let window = client.open_window(0xff0000);
    let constraints = client.bind::<ZwpPointerConstraintsV1>(1..=1);
    compositor.pointer_at(100.0, 100.0);
    client.take_input();
    (compositor, client, constraints, window)
}

impl Compositor {
    pub fn pointer_delta(&self, dx: f64, dy: f64) {
        self.with(move |state| {
            let delta = (dx, dy).into();
            let raw = (dx * 2.0, dy * 2.0).into();
            state.on_pointer_delta(delta, raw, InputTime::now());
        });
    }

    /// An absolute device (the nested backend's host cursor) at `x`, `y`.
    pub fn pointer_absolute(&self, x: f64, y: f64) {
        self.with(move |state| state.on_pointer_absolute((x, y).into(), InputTime::now()));
    }

    pub fn pointer_location(&self) -> (f64, f64) {
        self.with(|state| {
            let location = state.seat.get_pointer().unwrap().current_location();
            (location.x, location.y)
        })
    }
}

/// Asks for a token, with the serial of the latest key press if `serial` is set.
pub fn activation_token(client: &mut Client, serial: bool, surface: &WlSurface) -> String {
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

/// A compositor with a running X server and an X client of it, or `None` on a machine without
/// Xwayland, where there is nothing to test.
pub fn with_x_server() -> Option<(Compositor, RustConnection, u32, MutexGuard<'static, ()>)> {
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
pub fn x_window(connection: &RustConnection, root: u32, override_redirect: bool) -> u32 {
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

pub fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Where a window is on the screen. Its parent is the frame of the window manager.
pub fn x_geometry(connection: &RustConnection, window: u32) -> (i32, i32, u32, u32) {
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
pub fn kill_x_server(display: u32) {
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

/// The XRGB pixel at (x, y) of a captured image `width` wide.
pub fn captured_pixel(file: &fs::File, width: i32, x: i32, y: i32) -> u32 {
    use std::os::unix::fs::FileExt;
    let mut pixel = [0; 4];
    file.read_exact_at(&mut pixel, ((y * width + x) * 4) as u64)
        .unwrap();
    u32::from_le_bytes(pixel) & 0xff_ffff
}

/// Captures all of the first output by `wlr-screencopy` and returns the image.
pub fn screencopy(compositor: &Compositor, client: &mut Client, cursor: i32) -> fs::File {
    let manager: ZwlrScreencopyManagerV1 = client.bind(1..=3);
    let output: WlOutput = client.bind(1..=4);
    let frame = manager.capture_output(cursor, &output, &client.handle, ());
    let (width, height) = client.describe_frame();
    assert_eq!((width, height), (WIDTH, HEIGHT));
    let (buffer, file) = client.capture_buffer(width, height);
    client.state.copy = CopyState::default();
    frame.copy(&buffer);
    client.sync();
    assert!(!client.state.copy.ready, "answered before the next frame");
    compositor.run_captures(0);
    client.sync();
    assert!(client.state.copy.ready, "no ready");
    assert!(!client.state.copy.failed);
    frame.destroy();
    file
}

pub fn foreign_manager(client: &mut Client) -> ZwlrForeignToplevelManagerV1 {
    let manager = client.bind(1..=3);
    client.sync();
    manager
}

/// A clipboard manager: a client without any window.
pub struct Manager {
    pub client: Client,
    pub _device: ZwlrDataControlDeviceV1,
    pub control: ZwlrDataControlManagerV1,
}

impl Manager {
    pub fn connect(compositor: &Compositor) -> Self {
        let mut client = Client::connect(compositor);
        let control: ZwlrDataControlManagerV1 = client.bind(1..=2);
        let seat = client.seat();
        let device = control.get_data_device(&seat, &client.handle, ());
        client.sync();
        Self {
            client,
            _device: device,
            control,
        }
    }
}

pub const KEY_RIGHTSHIFT: u32 = 54;

pub const REPEATING: &str = "repeat_delay = 40
    repeat_rate = 200
    bind = SUPER, Right, workspace, next
    bind = SUPER, Return, spawn, true
    bind = SUPER, w, close\n";

pub fn ipc_lines(compositor: &Compositor, request: &str) -> BufReader<UnixStream> {
    let socket = compositor.with(|state| state.socket_name.clone());
    let path = crate::ipc::socket_path(socket.as_ref()).unwrap();
    let mut stream = UnixStream::connect(path).unwrap();
    writeln!(stream, "{request}").unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    BufReader::new(stream)
}

pub fn next_line(reader: &mut BufReader<UnixStream>) -> String {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    line
}

/// A tile that draws up to the edge of the output, across the gap it was told to leave.
pub fn overflowing_tile() -> (Compositor, Client, usize) {
    let compositor = Compositor::start_with("gap_outer = 40\nborder_width = 0\n");
    let mut client = Client::connect(&compositor);
    let tile = client.open_window(RED);
    client.sync();
    client.draw_sized(tile, (WIDTH, HEIGHT), RED);
    client.sync();
    client.take_input();
    (compositor, client, tile)
}

impl Client {
    /// `sync` for a client that may have earned a protocol error, which ends it and only it.
    pub fn sync_or_die(&mut self) {
        let _ = self.queue.roundtrip(&mut self.state);
    }
}

/// Whatever a client does to itself, the compositor stays and serves the next one.
pub fn survives(compositor: &Compositor) {
    let mut client = Client::connect(compositor);
    let window = client.open_window(BLUE);
    client.sync();
    client.close(window);
    client.sync();
}

// Plugins: what they ask for on the socket, and `wirbel plug`.

/// Waits until `done` holds, or fails the test.
pub fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < ANSWER, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn first_window_id(ipc: &mut RawIpc) -> u64 {
    let windows = ipc.request("windows");
    let start = windows.find(r#""id":"#).expect("no window") + 5;
    let digits: String = windows[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap()
}

/// A config file and a plugin script for the `wirbel plug` tests. The script notes that it ran,
/// then waits to be stopped.
pub struct PlugSetup {
    pub folder: PathBuf,
    pub config: PathBuf,
    pub plugin: PathBuf,
    pub marker: PathBuf,
}

impl PlugSetup {
    pub fn new(name: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let folder = scratch_path(name);
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        let config = folder.join("config");
        fs::write(&config, "# mine\nsloppy_focus = 0\n").unwrap();
        let plugin = folder.join("marker.sh");
        let marker = folder.join("ran");
        fs::write(
            &plugin,
            format!(
                "#!/bin/sh\necho \"$WIRBEL_PLUGIN $WIRBEL_SOCKET\" > '{}'\nexec sleep 60\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&plugin, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            folder,
            config,
            plugin,
            marker,
        }
    }

    pub fn plug(&self, compositor: &Compositor, args: &[&str]) -> String {
        let output = compositor
            .cli(&[&["plug"], args].concat())
            .env("WIRBEL_CONFIG", &self.config)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{args:?}: {stderr}");
        String::from_utf8(output.stdout).unwrap()
    }

    pub fn plug_failing(&self, compositor: &Compositor, args: &[&str]) -> String {
        let output = compositor
            .cli(&[&["plug"], args].concat())
            .env("WIRBEL_CONFIG", &self.config)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        String::from_utf8(output.stderr).unwrap()
    }

    pub fn running(&self, compositor: &Compositor) -> bool {
        let plugin = self.plugin.clone();
        compositor.with(move |state| state.plugins.running().any(|path| path == plugin))
    }
}

impl Drop for PlugSetup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.folder);
    }
}

pub const CTRL_ALT: &[u32] = &[KEY_LEFTCTRL, KEY_LEFTALT];

pub const KEY_ESC: u32 = 1;

pub fn hold(compositor: &Compositor, keys: &[u32], key_state: KeyState) {
    for key in keys {
        compositor.key(*key, key_state);
    }
}

/// A plugin connection that took Ctrl+Alt+left, with events on it.
pub fn grabbing_plugin(compositor: &Compositor) -> RawIpc {
    let mut plugin = compositor.raw_ipc();
    assert_eq!(plugin.request("grab ctrl+alt btn_left"), "ok");
    plugin
}

/// A closed connection is noticed in its own time.
pub fn wait_until_plugin_input_unused(compositor: &Compositor) {
    for _ in 0..500 {
        if !compositor.with(|state| state.plugin_input.in_use()) {
            return;
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("the grabs and lines of a closed connection stay");
}

/// Moves to `from`, presses, moves to `to` in a few steps and lets go.
pub fn stroke(compositor: &Compositor, from: (f64, f64), to: (f64, f64)) {
    compositor.pointer_at(from.0, from.1);
    compositor.button(BTN_LEFT, ButtonState::Pressed);
    for step in 1..=4 {
        let t = f64::from(step) / 4.0;
        compositor.pointer_at(from.0 + t * (to.0 - from.0), from.1 + t * (to.1 - from.1));
    }
    compositor.button(BTN_LEFT, ButtonState::Released);
}
