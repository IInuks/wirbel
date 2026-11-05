//! `wirbel check`: everything the DRM backend depends on, looked at without taking anything over.
//! No session gets activated and libseat is not even called, because opening a seat activates the
//! session and takes control of it. Instead this walks the same way libseat's logind backend goes,
//! up to the step before. Cards are opened for reading what the kernel already knows: no forced
//! connector probe, no modeset, and if the kernel hands out DRM master because nobody holds it,
//! it goes straight back. The render test only uses render nodes, like any client.

use std::{
    env,
    error::Error,
    ffi::{CStr, CString, c_char, c_int, c_uint, c_void},
    fs::{self, File, OpenOptions},
    os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    ptr,
};

use smithay::{
    backend::{
        allocator::{
            Allocator as _, Fourcc, Modifier,
            dmabuf::AsDmabuf,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{DrmDeviceFd, DrmNode, NodeType},
        egl::{EGLContext, EGLDevice, EGLDisplay},
        renderer::{
            Bind, ExportMem, Frame as _, ImportDma, Renderer as _,
            gles::{GlesRenderer, ffi},
            multigpu::{GpuManager, gbm::GbmGlesBackend},
        },
        udev::all_gpus,
    },
    reexports::drm::{
        Device as BasicDevice,
        control::{Device as ControlDevice, connector},
    },
    utils::{DeviceFd, Rectangle, Transform},
};

use crate::{
    backend::drm::{connector_name, mode_info, preferred_gpu},
    config::Config,
    cursor::Cursors,
    monitor::{self, ModeInfo},
    safety,
};

const SEAT: &str = "seat0";
const DRM_IOCTL_AUTH_MAGIC: libc::c_ulong = 0x4004_6411;
/// What the render test draws, in RGBA bytes.
const COLOUR: [u8; 3] = [255, 128, 0];
const SIZE: i32 = 64;

/// Counts what stands in the way of starting.
#[derive(Debug, Default)]
struct Report {
    problems: usize,
}

impl Report {
    fn section(&self, name: &str) {
        println!("\n{name}");
    }

    fn fine(&self, text: impl AsRef<str>) {
        println!("  ok    {}", text.as_ref());
    }

    fn note(&self, text: impl AsRef<str>) {
        println!("  note  {}", text.as_ref());
    }

    fn fail(&mut self, text: impl AsRef<str>) {
        self.problems += 1;
        println!("  FAIL  {}", text.as_ref());
    }
}

type SessionValue = unsafe extern "C" fn(*const c_char, *mut *mut c_char) -> c_int;
type SessionVt = unsafe extern "C" fn(*const c_char, *mut c_uint) -> c_int;
type SessionFlag = unsafe extern "C" fn(*const c_char) -> c_int;
type PidSession = unsafe extern "C" fn(libc::pid_t, *mut *mut c_char) -> c_int;
type UidDisplay = unsafe extern "C" fn(libc::uid_t, *mut *mut c_char) -> c_int;
type BusDefault = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type BusCall = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut BusError,
    *mut *mut c_void,
    *const c_char,
    ...
) -> c_int;
type BusRead = unsafe extern "C" fn(*mut c_void, *const c_char, ...) -> c_int;
type BusErrorFree = unsafe extern "C" fn(*mut BusError);

#[repr(C)]
struct BusError {
    name: *const c_char,
    message: *const c_char,
    need_free: c_int,
}

/// The C library libseat itself talks to the login manager with. It differs by stratum, so it
/// is looked up at runtime: libelogind in Void, libsystemd in Arch.
struct Login {
    library: &'static str,
    handle: *mut c_void,
}

impl Login {
    fn open() -> Option<Self> {
        let names: [(&str, &CStr); 2] = [
            ("libelogind", c"libelogind.so.0"),
            ("libsystemd", c"libsystemd.so.0"),
        ];
        names.into_iter().find_map(|(library, name)| {
            // Safety: loading a system library. Its initialisers only set up logging.
            let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            (!handle.is_null()).then_some(Self { library, handle })
        })
    }

    /// # Safety
    /// `F` has to be the type of the function called `name`.
    unsafe fn symbol<F: Copy>(&self, name: &CStr) -> Option<F> {
        // Safety: looking up a name. The cast is up to the caller.
        let symbol = unsafe { libc::dlsym(self.handle, name.as_ptr()) };
        // Safety: `F` is a function pointer, as promised by the caller.
        (!symbol.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, F>(&symbol) })
    }

    fn string(&self, name: &CStr, session: &CStr) -> Option<String> {
        // Safety: all functions asked for here have this type.
        let function = unsafe { self.symbol::<SessionValue>(name)? };
        let mut value = ptr::null_mut();
        // Safety: the function fills in a malloc'ed string on success.
        unsafe { (function(session.as_ptr(), &mut value) >= 0).then(|| take_string(value)) }
    }

    fn is_active(&self, session: &CStr) -> Option<c_int> {
        // Safety: the name matches the type.
        let function = unsafe { self.symbol::<SessionFlag>(c"sd_session_is_active")? };
        // Safety: a valid C string.
        Some(unsafe { function(session.as_ptr()) })
    }

    fn vt(&self, session: &CStr) -> Option<c_uint> {
        // Safety: the name matches the type.
        let function = unsafe { self.symbol::<SessionVt>(c"sd_session_get_vt")? };
        let mut vt = 0;
        // Safety: a valid C string and a place for the number.
        (unsafe { function(session.as_ptr(), &mut vt) } >= 0).then_some(vt)
    }

    /// The session of this process or, failing that, the graphical one of this user.
    fn own_session(&self) -> Option<(String, &'static str)> {
        let mut value = ptr::null_mut();
        // Safety: the names match the types, the functions fill in a malloc'ed string.
        unsafe {
            let by_pid = self.symbol::<PidSession>(c"sd_pid_get_session")?;
            if by_pid(libc::getpid(), &mut value) >= 0 {
                return Some((take_string(value), "the session of this process"));
            }
            let by_uid = self.symbol::<UidDisplay>(c"sd_uid_get_display")?;
            if by_uid(libc::getuid(), &mut value) >= 0 {
                return Some((take_string(value), "the display session of this user"));
            }
        }
        None
    }

    /// Asks the login manager over the system bus. libseat's next step is the same call, its
    /// step after that, `Activate`, is what this tool must not do.
    fn session_object(&self, session: &CStr) -> Result<String, String> {
        // Safety: the names match the types. Pointers are valid for the time of each call, the
        // bus connection stays open until the process ends, as for any `sd_bus_default_system`
        // that is not unreferenced.
        unsafe {
            let missing = || "the library lacks sd-bus".to_string();
            let default: BusDefault = self.symbol(c"sd_bus_default_system").ok_or_else(missing)?;
            let call: BusCall = self.symbol(c"sd_bus_call_method").ok_or_else(missing)?;
            let read: BusRead = self.symbol(c"sd_bus_message_read").ok_or_else(missing)?;
            let free: BusErrorFree = self.symbol(c"sd_bus_error_free").ok_or_else(missing)?;

            let mut bus = ptr::null_mut();
            let code = default(&mut bus);
            if code < 0 {
                return Err(format!("no system bus: {}", errno(-code)));
            }
            let mut error = BusError {
                name: ptr::null(),
                message: ptr::null(),
                need_free: 0,
            };
            let mut reply = ptr::null_mut();
            let code = call(
                bus,
                c"org.freedesktop.login1".as_ptr(),
                c"/org/freedesktop/login1".as_ptr(),
                c"org.freedesktop.login1.Manager".as_ptr(),
                c"GetSession".as_ptr(),
                &mut error,
                &mut reply,
                c"s".as_ptr(),
                session.as_ptr(),
            );
            if code < 0 {
                let message = if error.message.is_null() {
                    errno(-code)
                } else {
                    CStr::from_ptr(error.message).to_string_lossy().into_owned()
                };
                free(&mut error);
                return Err(message);
            }
            let mut path: *const c_char = ptr::null();
            if read(reply, c"o".as_ptr(), &mut path) < 0 || path.is_null() {
                return Err("unreadable answer".to_string());
            }
            Ok(CStr::from_ptr(path).to_string_lossy().into_owned())
        }
    }
}

/// # Safety
/// `value` has to be a string from malloc.
unsafe fn take_string(value: *mut c_char) -> String {
    // Safety: as promised by the caller.
    unsafe {
        let string = CStr::from_ptr(value).to_string_lossy().into_owned();
        libc::free(value.cast());
        string
    }
}

fn errno(code: c_int) -> String {
    std::io::Error::from_raw_os_error(code).to_string()
}

/// `KEY=value` from the file the login manager keeps for a session.
fn session_file_value(session: &str, key: &str) -> Option<String> {
    let text = fs::read_to_string(format!("/run/systemd/sessions/{session}")).ok()?;
    let prefix = format!("{key}=");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::to_string)
}

fn check_session(report: &mut Report) {
    report.section("Session (the way libseat goes, up to the step that would activate it)");
    match env::var("LIBSEAT_BACKEND") {
        Ok(backend) => report.note(format!("LIBSEAT_BACKEND={backend}")),
        Err(_) => report.note("LIBSEAT_BACKEND not set: libseat tries seatd, then logind"),
    }
    if Path::new("/run/seatd.sock").exists() {
        report.fine("seatd is running, libseat would take that");
    } else {
        report.note("no seatd running, libseat goes on to logind");
    }
    match env::var_os("XDG_RUNTIME_DIR").filter(|dir| Path::new(dir).is_dir()) {
        Some(_) => report.fine("XDG_RUNTIME_DIR is there, for the Wayland socket"),
        None => report.fail("XDG_RUNTIME_DIR is missing, there would be no Wayland socket"),
    }
    let Some(login) = Login::open() else {
        report.fail("neither libelogind nor libsystemd can be loaded here");
        return;
    };
    report.fine(format!("login library: {}", login.library));

    let from_env = env::var("XDG_SESSION_ID").ok().filter(|id| !id.is_empty());
    let session = match from_env {
        Some(id) => Some((id, "XDG_SESSION_ID")),
        None => login.own_session(),
    };
    let Some((session, source)) = session else {
        report.fail("no session found. On a TTY: log in there, do not come from su or a service");
        return;
    };
    let Ok(id) = CString::new(session.as_str()) else {
        report.fail("the session id is not a string");
        return;
    };
    let active = login.is_active(&id);
    if active.is_none_or(|active| active < 0) {
        report.fail(format!(
            "session {session} ({source}) is unknown to {}: {}",
            login.library,
            active.map_or("no such function".to_string(), |code| errno(-code)),
        ));
        return;
    }
    let seat = login.string(c"sd_session_get_seat", &id);
    let kind = login.string(c"sd_session_get_type", &id);
    let class = login.string(c"sd_session_get_class", &id);
    let vt = login.vt(&id);
    report.fine(format!(
        "session {session} from {source}: seat {}, vt {}, type {}, class {}, active {}",
        seat.as_deref().unwrap_or("none"),
        vt.map_or("none".to_string(), |vt| vt.to_string()),
        kind.as_deref().unwrap_or("?"),
        class.as_deref().unwrap_or("?"),
        if active == Some(1) { "yes" } else { "no" },
    ));
    if seat.is_none() {
        report.fail("the session has no seat, libseat would refuse it");
    }
    match login.session_object(&id) {
        Ok(path) => report.fine(format!("the login manager knows it over D-Bus: {path}")),
        Err(err) => report.fail(format!("the login manager does not answer: {err}")),
    }
    if let Some(controller) = session_file_value(&session, "CONTROLLER") {
        report.note(format!(
            "D-Bus client {controller} controls this session, a second compositor could not \
             take it. Normal inside a running desktop: on a free TTY that session is a new one"
        ));
    }
    let active_vt = fs::read_to_string("/sys/class/tty/tty0/active");
    if let Ok(vt) = active_vt {
        report.note(format!("active terminal: {}", vt.trim()));
    }
    if safety::is_virtual_terminal(0) {
        report.fine("stdin is a virtual terminal, wirbel would start the DRM backend");
    } else {
        report.note("stdin is no virtual terminal, wirbel would not start the DRM backend here");
    }
}

/// A DRM device for looking only.
struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl BasicDevice for Card {}
impl ControlDevice for Card {}

impl Card {
    /// Opens a card. The kernel makes the first opener of an unclaimed card its master: that
    /// goes back at once, nothing here needs it.
    fn open(path: &Path) -> std::io::Result<(Self, bool)> {
        let card = Self(OpenOptions::new().read(true).open(path)?);
        // Authenticating is for masters only, which is how libdrm tells.
        let mut magic: u32 = 0;
        // Safety: DRM_IOCTL_AUTH_MAGIC reads one u32.
        let answer = unsafe { libc::ioctl(card.0.as_raw_fd(), DRM_IOCTL_AUTH_MAGIC, &mut magic) };
        let refused =
            answer < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EACCES);
        let master = !refused;
        if master {
            let _ = card.release_master_lock();
        }
        Ok((card, master))
    }
}

fn describe(mode: &ModeInfo) -> String {
    format!(
        "{}x{}@{:.3}Hz",
        mode.width,
        mode.height,
        f64::from(mode.refresh_mhz) / 1000.0
    )
}

fn check_connector(report: &mut Report, config: &Config, info: &connector::Info) -> bool {
    let name = connector_name(info);
    let state = info.state();
    if state != connector::State::Connected {
        report.note(format!("{name}: {}", format!("{state:?}").to_lowercase()));
        return false;
    }
    // Without a forced probe the kernel hands out what it has, which is what the output runs
    // on. An output nobody has looked at yet shows no modes here, the backend will probe it.
    let modes: Vec<ModeInfo> = info.modes().iter().map(mode_info).collect();
    let rule = config.monitor_rule(&name);
    match monitor::pick_mode(&modes, rule) {
        Some(choice) => {
            report.fine(format!(
                "{name}: connected, {} modes, would use {}{}",
                modes.len(),
                describe(&modes[choice.index]),
                if rule.is_some() {
                    " by monitorrule"
                } else {
                    ""
                },
            ));
            if let Some(warning) = choice.warning {
                report.fail(format!("{name}: monitorrule not met: {warning}"));
            }
        }
        None => report.note(format!("{name}: connected, modes unknown until a probe")),
    }
    let mut rates: Vec<String> = modes
        .iter()
        .filter(|mode| !mode.interlaced)
        .filter(|mode| mode.width >= 2560)
        .map(describe)
        .collect();
    rates.dedup();
    rates.truncate(12);
    if !rates.is_empty() {
        report.note(format!("{name}: {}", rates.join(" ")));
    }
    true
}

/// A card, and whether a monitor hangs on it.
type CardInfo = (PathBuf, bool);

fn check_cards(report: &mut Report, config: &Config) -> Vec<CardInfo> {
    report.section("Cards and outputs (read only, no forced probe)");
    let cards = match all_gpus(SEAT) {
        Ok(cards) => cards,
        Err(err) => {
            report.fail(format!("udev does not answer: {err}"));
            return Vec::new();
        }
    };
    if cards.is_empty() {
        report.fail(format!("udev knows no cards on {SEAT}"));
    }
    let preferred = preferred_gpu(SEAT, config.render_gpu.as_deref());
    let mut found = Vec::new();
    for path in &cards {
        let suffix = if preferred.as_deref() == Some(path) {
            ", would render"
        } else {
            ""
        };
        found.push((path.clone(), false));
        let (card, master) = match Card::open(path) {
            Ok(card) => card,
            Err(err) => {
                report.fail(format!("{}: {err}", path.display()));
                continue;
            }
        };
        let driver = card.get_driver().map_or("?".to_string(), |driver| {
            driver.name().to_string_lossy().into_owned()
        });
        report.fine(format!("{}: {driver}{suffix}", path.display()));
        if master {
            report.note("nobody held this card. The kernel made us master, we let go at once");
        }
        let resources = match card.resource_handles() {
            Ok(resources) => resources,
            Err(err) => {
                report.fail(format!("{}: {err}", path.display()));
                continue;
            }
        };
        report.note(format!(
            "{} CRTCs, {} connectors",
            resources.crtcs().len(),
            resources.connectors().len()
        ));
        for handle in resources.connectors() {
            match card.get_connector(*handle, false) {
                Ok(info) => {
                    if check_connector(report, config, &info)
                        && let Some(last) = found.last_mut()
                    {
                        last.1 = true;
                    }
                }
                Err(err) => report.fail(format!("{handle:?}: {err}")),
            }
        }
    }
    if !found.iter().any(|(_, connected)| *connected) {
        report.fail("no connected output: the DRM backend would refuse to start");
    }
    found
}

fn render_node(card: &Path) -> Result<DrmNode, Box<dyn Error>> {
    let node = DrmNode::from_path(card)?
        .node_with_type(NodeType::Render)
        .ok_or("the card has no render node")??;
    Ok(node)
}

fn open_gbm(node: DrmNode) -> Result<GbmDevice<DrmDeviceFd>, Box<dyn Error>> {
    let path = node.dev_path().ok_or("no path for the render node")?;
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let fd = DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(file)));
    Ok(GbmDevice::new(fd)?)
}

/// Says what kind of GPU EGL finds here.
fn describe_gpu(gbm: &GbmDevice<DrmDeviceFd>) -> Result<(String, bool, usize), Box<dyn Error>> {
    // Safety: the display does not outlive this function, and neither does the renderer.
    let display = unsafe { EGLDisplay::new(gbm.clone())? };
    let software = EGLDevice::device_for_display(&display)?.is_software();
    let context = EGLContext::new(&display)?;
    // Safety: the context is current on this thread only.
    let mut renderer = unsafe { GlesRenderer::new(context)? };
    let name = renderer.with_context(|gl| {
        // Safety: the context is current, GetString returns a static string or null.
        unsafe {
            let name = gl.GetString(ffi::RENDERER);
            if name.is_null() {
                "unknown".to_string()
            } else {
                CStr::from_ptr(name.cast()).to_string_lossy().into_owned()
            }
        }
    })?;
    let formats = renderer.dmabuf_formats().iter().count();
    Ok((name, software, formats))
}

type Gpus = GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>;

/// Draws on `render`, into a buffer of `target`, and reads it back on `target`. The way a frame
/// goes in the DRM backend, minus the last step to the screen.
fn render_test(
    gpus: &mut Gpus,
    render: DrmNode,
    target: DrmNode,
    gbm: &GbmDevice<DrmDeviceFd>,
) -> Result<[u8; 3], Box<dyn Error>> {
    let modifiers: Vec<Modifier> = gpus
        .single_renderer(&target)?
        .as_ref()
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .filter(|format| format.code == Fourcc::Argb8888)
        .map(|format| format.modifier)
        .collect();
    let mut allocator = GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING);
    let size = SIZE as u32;
    let buffer = allocator.create_buffer(size, size, Fourcc::Argb8888, &modifiers)?;
    let mut dmabuf = buffer.export()?;

    {
        let mut renderer = if render == target {
            gpus.single_renderer(&render)?
        } else {
            gpus.renderer(&render, &target, Fourcc::Argb8888)?
        };
        let mut framebuffer = renderer.bind(&mut dmabuf)?;
        let [r, g, b] = COLOUR.map(|channel| f32::from(channel) / 255.0);
        let mut frame = renderer.render(&mut framebuffer, dmabuf_size(), Transform::Normal)?;
        let area = Rectangle::from_size(dmabuf_size());
        frame.clear([r, g, b, 1.0].into(), &[area])?;
        let sync = frame.finish()?;
        drop(framebuffer);
        renderer.wait(&sync)?;
    }

    let mut renderer = gpus.single_renderer(&target)?;
    let framebuffer = renderer.bind(&mut dmabuf)?;
    let area = Rectangle::from_size((SIZE, SIZE).into());
    let mapping = renderer.copy_framebuffer(&framebuffer, area, Fourcc::Abgr8888)?;
    drop(framebuffer);
    let pixels = renderer.map_texture(&mapping)?;
    let middle = (SIZE / 2 * SIZE + SIZE / 2) as usize * 4;
    Ok([pixels[middle], pixels[middle + 1], pixels[middle + 2]])
}

fn dmabuf_size() -> smithay::utils::Size<i32, smithay::utils::Physical> {
    (SIZE, SIZE).into()
}

fn matches_colour(pixel: [u8; 3]) -> bool {
    pixel
        .iter()
        .zip(COLOUR)
        .all(|(got, want)| got.abs_diff(want) <= 2)
}

fn check_render(report: &mut Report, config: &Config, cards: &[CardInfo]) {
    report.section("Rendering (EGL on render nodes, no display involved)");
    let mut usable = Vec::new();
    for (card, _) in cards {
        let node = match render_node(card) {
            Ok(node) => node,
            Err(err) => {
                report.fail(format!("{}: {err}", card.display()));
                continue;
            }
        };
        let gbm = match open_gbm(node) {
            Ok(gbm) => gbm,
            Err(err) => {
                report.note(format!("{node}: no GBM here: {err}"));
                continue;
            }
        };
        match describe_gpu(&gbm) {
            Ok((name, false, formats)) => {
                report.fine(format!("{node}: {name}, {formats} dmabuf formats"));
                usable.push((card.clone(), node, gbm));
            }
            Ok((name, true, _)) => report.note(format!("{node}: only in software: {name}")),
            Err(err) => report.note(format!(
                "{node}: no EGL here: {err}. For NVIDIA, this binary has to run in the Void \
                 stratum, the others have no userspace for it"
            )),
        }
    }
    for (card, connected) in cards {
        if *connected && !usable.iter().any(|(usable, ..)| usable == card) {
            report.fail(format!(
                "{} drives a monitor but has no working renderer in this stratum",
                card.display()
            ));
        }
    }
    let preferred = preferred_gpu(SEAT, config.render_gpu.as_deref());
    let chosen = usable
        .iter()
        .position(|(card, ..)| Some(card) == preferred.as_ref());
    let Some(chosen) = chosen.or((!usable.is_empty()).then_some(0)) else {
        report.fail("no GPU can render here");
        return;
    };

    let manager = GpuManager::new(GbmGlesBackend::default());
    let mut gpus: Gpus = match manager {
        Ok(gpus) => gpus,
        Err(err) => {
            report.fail(format!("no GPU manager: {err:?}"));
            return;
        }
    };
    for (_, node, gbm) in &usable {
        if let Err(err) = gpus.as_mut().add_node(*node, gbm.clone()) {
            report.fail(format!("{node}: {err}"));
        }
    }
    let render = usable[chosen].1;
    for (_, target, gbm) in &usable {
        let name = format!("drawn on {render}, into a buffer of {target}");
        match render_test(&mut gpus, render, *target, gbm) {
            Ok(pixel) if matches_colour(pixel) => report.fine(format!("{name}: picture correct")),
            Ok(pixel) => report.fail(format!("{name}: wrong picture, {pixel:?}")),
            Err(err) => report.fail(format!("{name}: {err}")),
        }
    }
}

fn check_config(report: &mut Report, path: Option<&Path>) -> Config {
    report.section("Configuration");
    let Some(path) = path else {
        report.note("no configuration path, using the defaults");
        return Config::default();
    };
    match Config::load(path) {
        Ok((config, errors)) => {
            report.fine(format!(
                "{}: {} monitorrules",
                path.display(),
                config.monitor_rules.len()
            ));
            for error in errors {
                report.fail(format!("{error}"));
            }
            config
        }
        Err(err) => {
            report.fail(format!("{}: {err}", path.display()));
            Config::default()
        }
    }
}

fn check_input(report: &mut Report, config: &Config) {
    report.section("Input and cursor");
    let keyboards = safety::readable_keyboards();
    if keyboards == 0 {
        report.fail(format!(
            "no keyboard readable: {} would only work while the main loop runs. Is the user in \
             the group input?",
            safety::CHORD_NAME
        ));
    } else {
        report.fine(format!(
            "{keyboards} keyboards readable for {}",
            safety::CHORD_NAME
        ));
    }
    let cursors = Cursors::load(config.cursor_theme.as_deref(), config.cursor_size);
    report.fine(cursors.describe());
    match fs::read_to_string("/proc/sys/kernel/sysrq") {
        Ok(value) => report.note(format!("kernel.sysrq = {}", value.trim())),
        Err(err) => report.note(format!("kernel.sysrq unknown: {err}")),
    }
}

/// Runs all checks. Returns whether nothing stands in the way of the DRM backend.
pub fn run(config_path: Option<&Path>) -> bool {
    let mut report = Report::default();
    println!("wirbel check: looks only, nothing is activated");
    check_session(&mut report);
    let config = check_config(&mut report, config_path);
    let cards = check_cards(&mut report, &config);
    check_render(&mut report, &config, &cards);
    check_input(&mut report, &config);
    if report.problems == 0 {
        println!("\nNo problems found.");
    } else {
        println!("\n{} problems found.", report.problems);
    }
    report.problems == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_check_allows_rounding_only() {
        assert!(matches_colour([255, 127, 1]));
        assert!(!matches_colour([0, 0, 0]));
    }
}
