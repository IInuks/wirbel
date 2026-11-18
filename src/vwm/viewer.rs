//! `wirbel vwm show NAME`: a normal window on the desktop that shows an invisible one live and
//! passes the keyboard and the pointer on to it.
//!
//! The viewer is a small Wayland client of the desktop. A thread asks the shadow for its
//! status and, whenever the picture changed, for a picture as bare RGBA rows
//! (`shadow shot PATH.rgba`), which the main thread draws into a shared memory buffer, scaled to
//! fit the window. What the window gets from its seat goes to the shadow as `inject` requests:
//! the pointer as `at` in pixels of the shadow, buttons, the wheel, keys by their evdev code, so
//! that the layout of the shadow, the same as the desktop's, makes the characters.
//!
//! The shadow does not change for being looked at: its size stays, the picture is scaled.
//! `vwm resize` gives it the size of the viewer if wanted.

use std::{
    collections::BTreeSet,
    fs::File,
    io::Write,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    time::Duration,
};

use smithay::input::keyboard::xkb;
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_pointer::{self, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::{self, WlSeat},
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

use super::{
    cli::{Scratch, Session},
    wait::Status,
};

/// How often the thread looks for a new picture while nothing changes.
const IDLE: Duration = Duration::from_millis(33);
/// The pause after a new picture: about 60 a second at most.
const BUSY: Duration = Duration::from_millis(16);
/// What one wheel notch scrolls on the desktop, as `inject wheel` counts it.
const NOTCH: f64 = 15.0;
/// The colour around a picture that does not fill the window, as XRGB.
const BACKGROUND: u32 = 0x0018_1818;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// What the picture thread tells the window.
enum News {
    Picture {
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    },
    Title(String),
    Gone(String),
}

/// Where the picture of the shadow lies in the window: the scale and the top left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    pub scale: f64,
    pub x: f64,
    pub y: f64,
}

impl Fit {
    /// The picture as large as the window allows, keeping its shape, in the middle.
    pub fn of(window: (usize, usize), picture: (usize, usize)) -> Self {
        let (ww, wh) = (window.0.max(1) as f64, window.1.max(1) as f64);
        let (pw, ph) = (picture.0.max(1) as f64, picture.1.max(1) as f64);
        let scale = (ww / pw).min(wh / ph);
        Self {
            scale,
            x: ((ww - pw * scale) / 2.0).floor(),
            y: ((wh - ph * scale) / 2.0).floor(),
        }
    }

    /// The place in the shadow under a place in the window, kept inside the picture.
    pub fn to_shadow(&self, (x, y): (f64, f64), picture: (usize, usize)) -> (f64, f64) {
        let inside = |value: f64, size: usize| value.clamp(0.0, size.saturating_sub(1) as f64);
        (
            inside((x - self.x) / self.scale, picture.0),
            inside((y - self.y) / self.scale, picture.1),
        )
    }
}

/// For each pixel along one side of the window: the two pixels of the picture it lies between
/// and how far towards the second, or `None` outside the picture.
fn samples(
    size: usize,
    offset: f64,
    scale: f64,
    picture: usize,
) -> Vec<Option<(usize, usize, u32)>> {
    (0..size)
        .map(|at| {
            let place = (at as f64 + 0.5 - offset) / scale;
            if place < 0.0 || place >= picture as f64 {
                return None;
            }
            let centre = (place - 0.5).clamp(0.0, (picture - 1) as f64);
            let first = centre.floor() as usize;
            let second = (first + 1).min(picture - 1);
            // In 256ths, so that the mixing stays in whole numbers.
            let towards = ((centre - first as f64) * 256.0).round() as u32;
            Some((first, second, towards))
        })
        .collect()
}

/// Draws RGBA rows of `picture` size into XRGB rows of `window` size, scaled to fit with
/// bilinear filtering: at the same size every pixel is copied as it is.
pub fn draw(target: &mut [u32], window: (usize, usize), rgba: &[u8], picture: (usize, usize)) {
    let fit = Fit::of(window, picture);
    let (ww, wh) = window;
    let (pw, ph) = picture;
    if pw == 0 || ph == 0 {
        target.fill(BACKGROUND);
        return;
    }
    let columns = samples(ww, fit.x, fit.scale, pw);
    let rows = samples(wh, fit.y, fit.scale, ph);
    let channel = |row: usize, column: usize, c: usize| rgba[(row * pw + column) * 4 + c] as u32;
    for (line, row) in target.chunks_exact_mut(ww).take(wh).zip(&rows) {
        let Some((top, bottom, down)) = *row else {
            line.fill(BACKGROUND);
            continue;
        };
        for (pixel, column) in line.iter_mut().zip(&columns) {
            let Some((left, right, across)) = *column else {
                *pixel = BACKGROUND;
                continue;
            };
            let mut colour = 0;
            for c in 0..3 {
                let upper =
                    channel(top, left, c) * (256 - across) + channel(top, right, c) * across;
                let lower =
                    channel(bottom, left, c) * (256 - across) + channel(bottom, right, c) * across;
                let value = (upper * (256 - down) + lower * down + (1 << 15)) >> 16;
                colour = colour << 8 | value.min(255);
            }
            *pixel = colour;
        }
    }
}

/// The request for a button of the desktop: by name for the usual three.
pub fn button_request(button: u32, down: bool) -> String {
    let name = match button {
        BTN_LEFT => "left".to_string(),
        BTN_RIGHT => "right".to_string(),
        BTN_MIDDLE => "middle".to_string(),
        other => other.to_string(),
    };
    format!("inject button {name} {}", if down { "down" } else { "up" })
}

/// A shared memory buffer the window shows.
struct Buffer {
    buffer: WlBuffer,
    // Kept so that the memory stays: the compositor maps the same file.
    _file: File,
    map: *mut u32,
    size: (usize, usize),
    busy: bool,
}

impl Buffer {
    fn new(
        shm: &WlShm,
        size: (usize, usize),
        index: usize,
        qh: &QueueHandle<Viewer>,
    ) -> Result<Self, String> {
        let bytes = size.0 * size.1 * 4;
        // SAFETY: a plain call; the name is a valid C string.
        let fd = unsafe { libc::memfd_create(c"wirbel-vwm-viewer".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: the descriptor was just made and belongs to nobody else.
        let file = unsafe { File::from(OwnedFd::from_raw_fd(fd)) };
        file.set_len(bytes as u64)
            .map_err(|err| format!("a buffer of {bytes} bytes: {err}"))?;
        // SAFETY: the file is as long as the mapping and lives as long as the buffer.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        let pool = shm.create_pool(file.as_fd(), bytes as i32, qh, ());
        let (w, h) = (size.0 as i32, size.1 as i32);
        let buffer = pool.create_buffer(0, w, h, w * 4, Format::Xrgb8888, qh, index);
        pool.destroy();
        Ok(Self {
            buffer,
            _file: file,
            map: map.cast(),
            size,
            busy: false,
        })
    }

    fn pixels(&mut self) -> &mut [u32] {
        // SAFETY: the mapping holds exactly this many pixels and only this buffer writes it.
        unsafe { std::slice::from_raw_parts_mut(self.map, self.size.0 * self.size.1) }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        // SAFETY: the mapping was made with this length in `new` and is not used after this.
        unsafe { libc::munmap(self.map.cast(), self.size.0 * self.size.1 * 4) };
    }
}

struct Viewer {
    name: String,
    input: Session,
    compositor: WlCompositor,
    shm: WlShm,
    surface: Option<WlSurface>,
    toplevel: Option<XdgToplevel>,
    configured: bool,
    /// The size the desktop gave the window, `None` while it leaves the choice to us.
    window: Option<(usize, usize)>,
    buffers: [Option<Buffer>; 2],
    picture: Option<(usize, usize, Vec<u8>)>,
    dirty: bool,
    /// The last place of the pointer in the window, not yet passed on.
    motion: Option<(f64, f64)>,
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    wheel: f64,
    title: String,
    closed: Option<String>,
    /// `--record`: where what the user does goes, as events for `wirbel vwm create macro`.
    recorder: Option<File>,
    /// The last place of the pointer in pixels of the shadow.
    pointer: (f64, f64),
    /// The keyboard of the desktop, to name the keys while recording.
    xkb: Option<xkb::State>,
}

impl Viewer {
    fn size(&self) -> (usize, usize) {
        match (self.window, &self.picture) {
            (Some(size), _) if size.0 > 0 && size.1 > 0 => size,
            (_, Some((w, h, _))) => (*w, *h),
            _ => (640, 400),
        }
    }

    fn send(&mut self, request: &str) {
        if self.closed.is_some() {
            return;
        }
        if let Err(err) = self.input.ask(request) {
            self.closed = Some(err);
        }
    }

    /// The pointer goes to its last place before anything that depends on where it is.
    fn send_motion(&mut self) {
        let Some(place) = self.motion.take() else {
            return;
        };
        let Some((w, h, _)) = &self.picture else {
            return;
        };
        let picture = (*w, *h);
        let fit = Fit::of(self.size(), picture);
        let (x, y) = fit.to_shadow(place, picture);
        self.pointer = (x, y);
        self.send(&format!("inject at {x:.1} {y:.1}"));
    }

    /// One line in the recording, after the time in milliseconds since 1970.
    fn record(&mut self, event: &str) {
        let Some(file) = &mut self.recorder else {
            return;
        };
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_millis());
        if writeln!(file, "{ms} {event}").is_err() {
            self.recorder = None;
        }
    }

    fn record_pointer(&mut self, what: &str) {
        if self.recorder.is_some() {
            let (x, y) = self.pointer;
            self.record(&format!("{what} {} {}", x.round(), y.round()));
        }
    }

    fn redraw(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        if !self.configured || !self.dirty {
            return Ok(());
        }
        let size = self.size();
        let Some(index) = (0..2).find(|i| self.buffers[*i].as_ref().is_none_or(|b| !b.busy)) else {
            // Both still on the screen: the next release draws.
            return Ok(());
        };
        if self.buffers[index].as_ref().is_none_or(|b| b.size != size) {
            self.buffers[index] = Some(Buffer::new(&self.shm, size, index, qh)?);
        }
        let buffer = self.buffers[index].as_mut().expect("made above");
        match &self.picture {
            Some((w, h, rgba)) => draw(buffer.pixels(), size, rgba, (*w, *h)),
            None => buffer.pixels().fill(BACKGROUND),
        }
        buffer.busy = true;
        let surface = self
            .surface
            .as_ref()
            .expect("made before the first configure");
        surface.attach(Some(&buffer.buffer), 0, 0);
        surface.damage_buffer(0, 0, size.0 as i32, size.1 as i32);
        surface.commit();
        self.dirty = false;
        Ok(())
    }

    fn set_title(&mut self, title: String) {
        self.title = title;
        let text = match self.title.is_empty() {
            true => format!("{} (invisible)", self.name),
            false => format!("{}: {} (invisible)", self.name, self.title),
        };
        if let Some(toplevel) = &self.toplevel {
            toplevel.set_title(text);
        }
    }

    /// Lets go of everything still held, so that nothing stays pressed in the shadow.
    fn release_all(&mut self) {
        for key in std::mem::take(&mut self.keys) {
            self.send(&format!("inject code {key} up"));
        }
        for button in std::mem::take(&mut self.buttons) {
            self.send(&button_request(button, false));
        }
    }
}

/// Runs the viewer of the shadow behind `name` on the desktop of `WAYLAND_DISPLAY` until its
/// window is closed or the shadow ends.
pub fn run(name: &str, record: Option<&Path>) -> Result<(), String> {
    let recorder = match record {
        Some(path) => Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|err| format!("{}: {err}", path.display()))?,
        ),
        None => None,
    };
    let display = super::cli::display_of(name)?;
    let input = Session::on(&display)?;
    let connection = Connection::connect_to_env()
        .map_err(|err| format!("no desktop to show the window on: {err}"))?;
    let (globals, mut queue): (_, EventQueue<Viewer>) = registry_queue_init(&connection)
        .map_err(|err| format!("the desktop did not answer: {err}"))?;
    let qh = queue.handle();
    fn missing(what: &'static str) -> impl Fn(wayland_client::globals::BindError) -> String {
        move |err| format!("the desktop has no {what}: {err}")
    }
    let compositor: WlCompositor = globals
        .bind(&qh, 4..=6, ())
        .map_err(missing("wl_compositor"))?;
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).map_err(missing("wl_shm"))?;
    let wm_base: XdgWmBase = globals
        .bind(&qh, 1..=5, ())
        .map_err(missing("xdg_wm_base"))?;
    // Version 5 at most: later ones add wheel events in 120ths that would count twice.
    let seat: Option<WlSeat> = globals.bind(&qh, 1..=5, ()).ok();

    let mut viewer = Viewer {
        name: name.to_string(),
        input,
        compositor,
        shm,
        surface: None,
        toplevel: None,
        configured: false,
        window: None,
        buffers: [None, None],
        picture: None,
        dirty: true,
        motion: None,
        keys: BTreeSet::new(),
        buttons: BTreeSet::new(),
        wheel: 0.0,
        title: String::new(),
        closed: None,
        recorder,
        pointer: (0.0, 0.0),
        xkb: None,
    };
    let surface = viewer.compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_app_id("wirbel-vwm-viewer".into());
    viewer.surface = Some(surface.clone());
    viewer.toplevel = Some(toplevel);
    viewer.set_title(String::new());
    surface.commit();
    let _seat = seat;

    let (sender, news) = mpsc::channel();
    let (wake_read, wake_write) = pipe()?;
    let thread_display = display.clone();
    std::thread::spawn(move || {
        let why = pictures(&thread_display, &sender, &wake_write)
            .err()
            .unwrap_or_else(|| "the invisible window ended".into());
        let _ = sender.send(News::Gone(why));
        wake(&wake_write);
    });

    let result = event_loop(&connection, &mut queue, &mut viewer, &news, &wake_read);
    viewer.release_all();
    result
}

fn event_loop(
    connection: &Connection,
    queue: &mut EventQueue<Viewer>,
    viewer: &mut Viewer,
    news: &Receiver<News>,
    wake_read: &OwnedFd,
) -> Result<(), String> {
    let qh = queue.handle();
    let failed = |err: &dyn std::fmt::Display| format!("the desktop connection broke: {err}");
    loop {
        queue.dispatch_pending(viewer).map_err(|err| failed(&err))?;
        viewer.send_motion();
        loop {
            match news.try_recv() {
                Ok(News::Picture {
                    width,
                    height,
                    rgba,
                }) => {
                    viewer.picture = Some((width, height, rgba));
                    viewer.dirty = true;
                }
                Ok(News::Title(title)) => viewer.set_title(title),
                Ok(News::Gone(why)) => viewer.closed = Some(why),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    viewer
                        .closed
                        .get_or_insert_with(|| "the picture thread ended".into());
                    break;
                }
            }
        }
        if let Some(why) = viewer.closed.take() {
            return match why.is_empty() {
                true => Ok(()),
                false => Err(why),
            };
        }
        viewer.redraw(&qh)?;
        connection.flush().map_err(|err| failed(&err))?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut fds = [
            libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_read.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two valid pollfd entries that live across the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("poll: {err}"));
        }
        if fds[0].revents != 0 {
            guard.read().map_err(|err| failed(&err))?;
        } else {
            drop(guard);
        }
        if fds[1].revents != 0 {
            let mut bytes = [0u8; 64];
            // SAFETY: reads into a buffer of that length from our own pipe.
            unsafe {
                libc::read(
                    wake_read.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
        }
    }
}

/// The picture thread: a new picture whenever the shadow drew something new.
fn pictures(display: &str, sender: &Sender<News>, wake_write: &OwnedFd) -> Result<(), String> {
    let mut session = Session::on(display)?;
    let scratch = Scratch::new("view.rgba")?;
    let mut last = None;
    let mut title = None;
    loop {
        let status = Status::parse(&session.ask("shadow status")?)?;
        let mut busy = false;
        if last != Some(status.damage) {
            let answer = session.ask(&format!("shadow shot\t{}", scratch.path()))?;
            let size = answer
                .split_once('x')
                .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)));
            let (width, height): (usize, usize) =
                size.ok_or_else(|| format!("unexpected answer `{answer}`"))?;
            let rgba = std::fs::read(scratch.file())
                .map_err(|err| format!("the picture of the window: {err}"))?;
            if rgba.len() == width * height * 4 {
                if sender
                    .send(News::Picture {
                        width,
                        height,
                        rgba,
                    })
                    .is_err()
                {
                    return Ok(());
                }
                wake(wake_write);
            }
            last = Some(status.damage);
            busy = true;
        }
        if title.as_ref() != Some(&status.title) {
            title = Some(status.title.clone());
            if sender.send(News::Title(status.title)).is_err() {
                return Ok(());
            }
            wake(wake_write);
        }
        std::thread::sleep(if busy { BUSY } else { IDLE });
    }
}

fn pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [0; 2];
    // SAFETY: fills two descriptors on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(format!("pipe: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: both were just made and belong to nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn wake(fd: &OwnedFd) {
    // SAFETY: writes one byte from a valid buffer; a full pipe already wakes the reader.
    unsafe { libc::write(fd.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
}

impl Dispatch<WlRegistry, GlobalListContents> for Viewer {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(Viewer: WlCompositor);
delegate_noop!(Viewer: WlShmPool);
delegate_noop!(Viewer: ignore WlShm);
delegate_noop!(Viewer: ignore WlSurface);

impl Dispatch<WlBuffer, usize> for Viewer {
    fn event(
        viewer: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(buffer) = viewer.buffers[*index].as_mut()
        {
            buffer.busy = false;
        }
    }
}

impl Dispatch<XdgWmBase, ()> for Viewer {
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

impl Dispatch<XdgSurface, ()> for Viewer {
    fn event(
        viewer: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            viewer.configured = true;
            viewer.dirty = true;
        }
    }
}

impl Dispatch<XdgToplevel, ()> for Viewer {
    fn event(
        viewer: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                let size = (width.max(0) as usize, height.max(0) as usize);
                viewer.window = (size.0 > 0 && size.1 > 0).then_some(size);
            }
            // Closing the viewer only hides the window again; it goes on invisibly.
            xdg_toplevel::Event::Close => viewer.closed = Some(String::new()),
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for Viewer {
    fn event(
        _: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(qh, ());
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for Viewer {
    fn event(
        viewer: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => viewer.motion = Some((surface_x, surface_y)),
            wl_pointer::Event::Leave { .. } => {
                viewer.send_motion();
                for button in std::mem::take(&mut viewer.buttons) {
                    viewer.send(&button_request(button, false));
                }
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(state),
                ..
            } => {
                viewer.send_motion();
                let down = state == wl_pointer::ButtonState::Pressed;
                match down {
                    true => viewer.buttons.insert(button),
                    false => viewer.buttons.remove(&button),
                };
                viewer.send(&button_request(button, down));
                let event = button_request(button, down);
                let mut words = event.split(' ').skip(2);
                if let (Some(name), Some(updown)) = (words.next(), words.next()) {
                    viewer.record_pointer(&format!("{updown} {name}"));
                }
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value,
                ..
            } => {
                viewer.send_motion();
                viewer.wheel += value / NOTCH;
                // Whole hundredths at least, so that a touchpad does not flood the shadow.
                if viewer.wheel.abs() >= 0.01 {
                    let steps = std::mem::take(&mut viewer.wheel);
                    viewer.send(&format!("inject wheel {steps:.3}"));
                    viewer.record_pointer(&format!("wheel {steps:.3}"));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for Viewer {
    fn event(
        viewer: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(state),
                ..
            } => {
                let down = state == wl_keyboard::KeyState::Pressed;
                // A key that went down before the window had the keyboard is not let go here.
                if !down && !viewer.keys.remove(&key) {
                    return;
                }
                if down {
                    viewer.keys.insert(key);
                    if viewer.recorder.is_some()
                        && let Some(event) = viewer.xkb.as_ref().and_then(|s| describe_key(s, key))
                    {
                        viewer.record(&event);
                    }
                }
                viewer.send(&format!(
                    "inject code {key} {}",
                    if down { "down" } else { "up" }
                ));
            }
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if viewer.recorder.is_none()
                    || format != WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)
                {
                    return;
                }
                let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                // SAFETY: the desktop hands a file of `size` bytes holding a keymap as text.
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(
                        &context,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                };
                viewer.xkb = keymap.ok().flatten().map(|keymap| xkb::State::new(&keymap));
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(state) = &mut viewer.xkb {
                    state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Leave { .. } => {
                for key in std::mem::take(&mut viewer.keys) {
                    viewer.send(&format!("inject code {key} up"));
                }
            }
            _ => {}
        }
    }
}

/// What a key that went down does, for the recording: `text CHARACTERS` for what it types,
/// `key COMBINATION` for the rest (`key Return`, `key CTRL+l`). Nothing for modifiers alone and
/// dead keys.
fn describe_key(state: &xkb::State, evdev: u32) -> Option<String> {
    let code = xkb::Keycode::new(evdev + 8);
    let sym = state.key_get_one_sym(code);
    if sym.is_modifier_key() || sym.raw() == 0 {
        return None;
    }
    let active = |name: &str| state.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE);
    let (ctrl, alt, logo, shift) = (
        active(xkb::MOD_NAME_CTRL),
        active(xkb::MOD_NAME_ALT),
        active(xkb::MOD_NAME_LOGO),
        active(xkb::MOD_NAME_SHIFT),
    );
    if !ctrl && !alt && !logo {
        let text = state.key_get_utf8(code);
        if !text.is_empty() && !text.chars().any(char::is_control) {
            return Some(format!("text {text}"));
        }
    }
    // The key as it is without Shift: CTRL+SHIFT+t, not CTRL+SHIFT+T.
    let keymap = state.get_keymap();
    let layout = state.key_get_layout(code);
    let base = keymap
        .key_get_syms_by_level(code, layout, 0)
        .first()
        .copied()
        .unwrap_or(sym);
    let name = xkb::keysym_get_name(base);
    if name.starts_with("dead_") || name.is_empty() {
        return None;
    }
    let mut combo = String::new();
    for (on, word) in [
        (ctrl, "CTRL+"),
        (shift, "SHIFT+"),
        (alt, "ALT+"),
        (logo, "SUPER+"),
    ] {
        if on {
            combo.push_str(word);
        }
    }
    Some(format!("key {combo}{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_fits_the_window_keeping_its_shape() {
        let fit = Fit::of((800, 800), (400, 200));
        assert_eq!(
            fit,
            Fit {
                scale: 2.0,
                x: 0.0,
                y: 200.0
            }
        );
        let fit = Fit::of((1280, 800), (1280, 800));
        assert_eq!(
            fit,
            Fit {
                scale: 1.0,
                x: 0.0,
                y: 0.0
            }
        );
        let fit = Fit::of((640, 800), (1280, 800));
        assert_eq!(fit.scale, 0.5);
        assert_eq!(fit.y, 200.0);
    }

    #[test]
    fn places_in_the_window_map_to_the_shadow() {
        let picture = (400, 200);
        let fit = Fit::of((800, 800), picture);
        assert_eq!(fit.to_shadow((100.0, 300.0), picture), (50.0, 50.0));
        // Above the picture and right of it: kept at its edge.
        assert_eq!(fit.to_shadow((900.0, 10.0), picture), (399.0, 0.0));
    }

    #[test]
    fn drawing_scales_and_fills_the_rest() {
        // Two by one pixels, red and blue, into a window of four by four.
        let rgba = [255, 0, 0, 255, 0, 0, 255, 255];
        let mut target = vec![0u32; 16];
        draw(&mut target, (4, 4), &rgba, (2, 1));
        assert_eq!(&target[..4], &[BACKGROUND; 4]);
        assert_eq!(&target[12..], &[BACKGROUND; 4]);
        // The outer columns are the pixels themselves, the inner ones mix them.
        let row = &target[4..8];
        assert_eq!((row[0], row[3]), (0xff0000, 0x0000ff));
        assert_eq!(row[1], 0xbf0040);
        assert_eq!(&target[8..12], row);
    }

    #[test]
    fn at_the_same_size_pixels_are_copied() {
        let rgba: Vec<u8> = (0..12u8).flat_map(|n| [n * 20, n, 255 - n, 255]).collect();
        let mut target = vec![0u32; 12];
        draw(&mut target, (4, 3), &rgba, (4, 3));
        for (n, pixel) in target.iter().enumerate() {
            let n = n as u32;
            assert_eq!(*pixel, (n * 20) << 16 | n << 8 | (255 - n));
        }
    }

    #[test]
    fn buttons_go_by_name_or_code() {
        assert_eq!(button_request(0x110, true), "inject button left down");
        assert_eq!(button_request(0x112, false), "inject button middle up");
        assert_eq!(button_request(0x113, true), "inject button 275 down");
    }
}
