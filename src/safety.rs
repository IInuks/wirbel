//! The safety net. A compositor on a VT can leave the user in front of a black screen, so getting
//! out must not depend on the main loop: a thread of its own ends the process when the deadline
//! passes, on SIGTERM and on the emergency chord, and every way out restores the terminal.
//!
//! - [`install_log`]: the log and stderr go to a file, flushed line by line.
//! - [`install_panic_hook`]: a panic ends up in that file, and the terminal is restored.
//! - [`Watchdog`]: `--timeout`, SIGTERM/SIGINT and reporting a stalled main loop. A clean stop gets
//!   [`GRACE`], then the process goes down the hard way.
//! - [`Chord`] and [`watch_keyboards`]: the emergency chord, read from the evdev devices. Those
//!   are opened a second time without grabbing them, so it works while the main loop hangs.

use std::{
    backtrace::Backtrace,
    env,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use smithay::reexports::calloop::LoopSignal;

/// How long a clean stop may take before the process exits regardless.
pub const GRACE: Duration = Duration::from_secs(5);
/// The main loop silent for this long gets a line in the log.
const STALL: Duration = Duration::from_secs(5);
/// How often the main loop has to report in for the watchdog to take it for alive.
pub const HEARTBEAT: Duration = Duration::from_secs(1);
/// How often the watchdog looks at the clock.
const TICK: Duration = Duration::from_millis(100);
/// How often the watcher looks for new keyboards.
const RESCAN: Duration = Duration::from_secs(2);

/// Exit status of a panic.
pub const EXIT_PANIC: i32 = 101;
/// Exit status of the emergency chord.
pub const EXIT_EMERGENCY: i32 = 70;
/// Exit status when a clean stop did not finish within [`GRACE`].
pub const EXIT_HUNG: i32 = 71;
/// Exit status of `wirbel restart` under wirbel-session, which then starts the session anew.
pub const EXIT_RESTART: i32 = 75;
/// Set by wirbel-session for its wirbel: a restart is the job of the script then.
pub const SESSION_ENV: &str = "WIRBEL_SESSION";

const KEY_BACKSPACE: u16 = 14;
const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_RIGHTALT: u16 = 100;

/// Ctrl+Alt+Shift+Backspace, in evdev codes: it works on any keymap, even a broken one.
pub const CHORD_NAME: &str = "Ctrl+Alt+Shift+Backspace";

/// Follows one keyboard and tells when the emergency chord goes down.
#[derive(Debug, Default, Clone)]
pub struct Chord {
    ctrl: [bool; 2],
    alt: [bool; 2],
    shift: [bool; 2],
}

impl Chord {
    /// Takes a key event and returns whether it completes the chord. Only the press of Backspace
    /// does, so holding the modifiers alone or a repeated key does not count twice.
    pub fn feed(&mut self, key: u16, pressed: bool) -> bool {
        match key {
            KEY_LEFTCTRL => self.ctrl[0] = pressed,
            KEY_RIGHTCTRL => self.ctrl[1] = pressed,
            KEY_LEFTALT => self.alt[0] = pressed,
            KEY_RIGHTALT => self.alt[1] = pressed,
            KEY_LEFTSHIFT => self.shift[0] = pressed,
            KEY_RIGHTSHIFT => self.shift[1] = pressed,
            KEY_BACKSPACE if pressed => {
                return [self.ctrl, self.alt, self.shift]
                    .iter()
                    .all(|sides| sides[0] || sides[1]);
            }
            _ => {}
        }
        false
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

// Kernel interfaces of linux/kd.h, linux/vt.h and linux/input.h.
const KDGETMODE: libc::c_ulong = 0x4B3B;
const KDSETMODE: libc::c_ulong = 0x4B3A;
const KD_TEXT: libc::c_int = 0;
const KDGKBMODE: libc::c_ulong = 0x4B44;
const KDSKBMODE: libc::c_ulong = 0x4B45;
const K_UNICODE: libc::c_int = 3;
const K_OFF: libc::c_int = 4;
const VT_SETMODE: libc::c_ulong = 0x5602;
const VT_AUTO: libc::c_char = 0;
const EV_KEY: u16 = 1;
const EV_KEY_BITS: usize = 0x300 / 8;

#[repr(C)]
#[derive(Default)]
struct VtMode {
    mode: libc::c_char,
    waitv: libc::c_char,
    relsig: libc::c_short,
    acqsig: libc::c_short,
    frsig: libc::c_short,
}

/// The state a virtual terminal had before we took over.
#[derive(Debug, Clone, Copy)]
struct Terminal {
    fd: RawFd,
    keyboard: libc::c_int,
}

static TERMINAL: OnceLock<Terminal> = OnceLock::new();
/// Copy of the original stderr, for the last words. The log file takes over fd 2.
static CONSOLE: AtomicI32 = AtomicI32::new(2);
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Whether `fd` is a virtual terminal. A pty does not know this ioctl.
pub fn is_virtual_terminal(fd: RawFd) -> bool {
    let mut mode: libc::c_int = 0;
    // Safety: KDGETMODE writes one int.
    unsafe { libc::ioctl(fd, KDGETMODE, &mut mode) == 0 }
}

/// Remembers the terminal on stdin, if that is a VT, so that it can be restored.
pub fn remember_terminal() {
    if !is_virtual_terminal(0) {
        return;
    }
    let mut keyboard: libc::c_int = K_UNICODE;
    // Safety: KDGKBMODE writes one int.
    let known = unsafe { libc::ioctl(0, KDGKBMODE, &mut keyboard) } == 0;
    // A crash may have left the keyboard switched off. Typing must work afterwards in any case.
    if !known || keyboard == K_OFF {
        keyboard = K_UNICODE;
    }
    // Safety: duplicating an open descriptor. The copy lives as long as the process.
    let fd = unsafe { libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 3) };
    if fd >= 0 {
        let _ = TERMINAL.set(Terminal { fd, keyboard });
    }
}

/// Returns the terminal to text mode. Only uses ioctls, so any thread may call it at any time.
/// elogind does the same once the session loses its controller, this is for the cases where that
/// is slow or the process does not get to die.
/// Points stderr back at the console it was before the log took it, for a program that comes next.
pub fn restore_stderr() {
    let console = CONSOLE.load(Ordering::Relaxed);
    if console != 2 {
        // Safety: plain descriptor shuffling, failing leaves stderr at the log.
        unsafe { libc::dup2(console, 2) };
    }
}

pub fn restore_terminal() {
    let Some(terminal) = TERMINAL.get() else {
        return;
    };
    let mode = VtMode {
        mode: VT_AUTO,
        ..VtMode::default()
    };
    // Safety: these ioctls take plain values or read the struct given, and failing is harmless.
    unsafe {
        libc::ioctl(terminal.fd, KDSKBMODE, terminal.keyboard);
        libc::ioctl(terminal.fd, KDSETMODE, KD_TEXT);
        libc::ioctl(terminal.fd, VT_SETMODE, &mode);
    }
}

/// Says something where the user can see it, whatever state the process is in.
pub fn say(message: &str) {
    let bytes = message.as_bytes();
    // Safety: writing a live buffer to a descriptor that at worst is closed.
    unsafe {
        libc::write(
            CONSOLE.load(Ordering::Relaxed),
            bytes.as_ptr().cast(),
            bytes.len(),
        );
    }
}

/// What has to go with the process, however it ends. A hard exit runs no destructors.
#[derive(Debug, Default)]
struct Leftovers {
    files: Vec<CString>,
    groups: Vec<libc::pid_t>,
}

static LEFTOVERS: Mutex<Leftovers> = Mutex::new(Leftovers {
    files: Vec::new(),
    groups: Vec::new(),
});

fn leftovers() -> std::sync::MutexGuard<'static, Leftovers> {
    LEFTOVERS.lock().unwrap_or_else(|err| err.into_inner())
}

/// A socket or lock file of ours. A later clean exit removes it itself, that does no harm.
pub fn remove_at_exit(path: &Path) {
    if let Ok(path) = CString::new(path.as_os_str().as_bytes()) {
        leftovers().files.push(path);
    }
}

/// A process group of the autostart, which ends with the session.
pub fn end_at_exit(group: libc::pid_t) {
    leftovers().groups.push(group);
}

/// Forgets a group that may be empty by now, before its number comes up for someone else.
pub fn group_left(group: libc::pid_t) {
    // Safety: signal 0 only asks whether the group exists.
    if unsafe { libc::kill(-group, 0) } != 0 {
        leftovers().groups.retain(|other| *other != group);
    }
}

fn end_groups(leftovers: &mut Leftovers) {
    for group in leftovers.groups.drain(..) {
        // Safety: the group is ours, see `group_left`.
        unsafe { libc::kill(-group, libc::SIGTERM) };
    }
}

/// The autostart ends, on a clean stop.
pub fn end_autostart() {
    end_groups(&mut leftovers());
}

/// Works from the panic hook and other threads. It does not wait for a lock: a thread may have died
/// holding it, and the screen matters more than a file.
fn clean_up() {
    if let Ok(mut leftovers) = LEFTOVERS.try_lock() {
        end_groups(&mut leftovers);
        for file in &leftovers.files {
            // Safety: a valid C string, and it is fine if the file is gone.
            unsafe { libc::unlink(file.as_ptr()) };
        }
    }
}

/// Ends the process right here: no destructors, nothing that could wait for the main loop. The
/// kernel closes the DRM devices and the session, which is what hands the screen back.
pub fn hard_exit(code: i32, reason: &str) -> ! {
    tracing::error!(code, reason, "hard exit");
    restore_terminal();
    clean_up();
    let log = LOG_PATH
        .get()
        .map(|path| format!(", log: {}", path.display()))
        .unwrap_or_default();
    say(&format!("\r\nwirbel: {reason}{log}\r\n"));
    // Safety: _exit is always sound.
    unsafe { libc::_exit(code) }
}

/// `$XDG_STATE_HOME/wirbel/wirbel.log`, or `~/.local/state/wirbel/wirbel.log`.
pub fn default_log_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })?;
    Some(base.join("wirbel").join("wirbel.log"))
}

fn open_log(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        fs::rename(path, old)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Writes every line straight to the file: there may be no later.
struct LogFile(Arc<File>);

impl Write for LogFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        (&*self.0).write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Starts logging: to stderr without a `path`, otherwise to that file, whose previous version
/// becomes `.old`. stderr follows into the file, along with what drivers and clients say there.
pub fn install_log(path: Option<&Path>) -> io::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("wirbel=info,warn"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let Some(path) = path else {
        builder.init();
        return Ok(());
    };
    let file = Arc::new(open_log(path)?);
    let _ = LOG_PATH.set(path.to_path_buf());
    // Safety: plain descriptor shuffling at a point where only this thread runs.
    unsafe {
        let console = libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3);
        if console >= 0 {
            CONSOLE.store(console, Ordering::Relaxed);
        }
        libc::dup2(file.as_raw_fd(), 2);
    }
    builder
        .with_ansi(false)
        .with_writer(move || LogFile(file.clone()))
        .init();
    Ok(())
}

/// A panic in any thread ends the compositor: it gets logged with a backtrace, and the terminal
/// returns to text mode. Unwinding is out of the question with the DRM state half done.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let backtrace = Backtrace::force_capture();
        tracing::error!("panic: {info}\n{backtrace}");
        hard_exit(EXIT_PANIC, &format!("panic: {info}"));
    }));
}

/// When the watchdog stops the compositor. Set by the signal handler, which may do little else.
static TERMINATE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    TERMINATE.store(true, Ordering::Relaxed);
}

/// What the main loop shares with the watchdog thread.
#[derive(Debug, Clone)]
pub struct Watchdog {
    beat: Arc<AtomicU64>,
    started: Instant,
}

impl Watchdog {
    /// Starts the thread. `deadline` is the time until the compositor stops by itself.
    pub fn start(signal: LoopSignal, deadline: Option<Duration>) -> Self {
        let watchdog = Self {
            beat: Arc::new(AtomicU64::new(0)),
            started: Instant::now(),
        };
        // Safety: the handler only stores into an atomic.
        unsafe {
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        }
        let this = watchdog.clone();
        let spawned = thread::Builder::new()
            .name("watchdog".into())
            .spawn(move || this.run(signal, deadline));
        if let Err(err) = spawned {
            tracing::error!(%err, "no watchdog thread: --timeout and SIGTERM will not work");
        }
        watchdog
    }

    /// To be called by every iteration of the main loop.
    pub fn beat(&self) {
        let now = self.started.elapsed().as_millis() as u64;
        self.beat.store(now, Ordering::Relaxed);
    }

    fn run(self, signal: LoopSignal, deadline: Option<Duration>) {
        let mut stopping: Option<Instant> = None;
        let mut stalled = false;
        loop {
            thread::sleep(TICK);
            let elapsed = self.started.elapsed();
            if stopping.is_none() {
                let timed_out = deadline.is_some_and(|deadline| elapsed >= deadline);
                let terminated = TERMINATE.load(Ordering::Relaxed);
                if timed_out || terminated {
                    let reason = if timed_out { "timeout" } else { "signal" };
                    tracing::info!(reason, "stopping");
                    signal.stop();
                    signal.wakeup();
                    stopping = Some(Instant::now());
                }
            }
            if let Some(since) = stopping
                && since.elapsed() >= GRACE
            {
                hard_exit(EXIT_HUNG, "the main loop did not stop in time");
            }

            // The loop wakes up every `HEARTBEAT`, no matter how quiet the desktop is.
            let last = Duration::from_millis(self.beat.load(Ordering::Relaxed));
            let silent = elapsed.saturating_sub(last);
            if silent >= STALL && !stalled {
                tracing::error!(?silent, "the main loop does not respond");
            } else if silent < STALL && stalled {
                tracing::warn!("the main loop responds again");
            }
            stalled = silent >= STALL;
        }
    }
}

/// One open keyboard of the emergency watcher.
struct Keyboard {
    path: PathBuf,
    file: File,
    chord: Chord,
}

/// Whether `file` is an input device with a Backspace key.
fn has_backspace(file: &File) -> bool {
    let mut bits = [0u8; EV_KEY_BITS];
    // EVIOCGBIT(EV_KEY, len): _IOC(_IOC_READ, 'E', 0x20 + EV_KEY, len).
    let request = (2 << 30) | ((EV_KEY_BITS as libc::c_ulong) << 16) | (0x45 << 8) | (0x20 + 1);
    // Safety: the kernel writes at most `EV_KEY_BITS` bytes.
    let read = unsafe { libc::ioctl(file.as_raw_fd(), request, bits.as_mut_ptr()) };
    read > 0 && bits[usize::from(KEY_BACKSPACE) / 8] & (1 << (KEY_BACKSPACE % 8)) != 0
}

/// `/dev/input/event*`, the devices of the emergency watcher.
pub fn keyboard_paths() -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir("/dev/input") else {
        return Vec::new();
    };
    let is_event = |name: &str| name.starts_with("event");
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_str().is_some_and(is_event))
        .map(|entry| entry.path())
        .collect()
}

fn open_keyboard(path: &Path) -> Option<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()
}

/// Number of keyboards the emergency watcher can read, for `wirbel check`.
pub fn readable_keyboards() -> usize {
    let paths = keyboard_paths();
    let files = paths.iter().filter_map(|path| open_keyboard(path));
    files.filter(has_backspace).count()
}

/// Whether the session has the screen. Keys typed on another VT are none of our business.
pub type SessionActive = Arc<AtomicBool>;

/// Watches `devices`, or all of `/dev/input`, for the emergency chord and calls `trigger`. The
/// thread only keeps which modifiers are held, per device. A device that returns no events, such
/// as a stale one, is dropped; new ones are picked up every [`RESCAN`].
pub fn watch_keyboards(
    devices: Option<Vec<PathBuf>>,
    active: SessionActive,
    trigger: impl Fn() + Send + 'static,
) -> io::Result<()> {
    thread::Builder::new()
        .name("emergency".into())
        .spawn(move || {
            let mut keyboards: Vec<Keyboard> = Vec::new();
            let mut scanned: Option<Instant> = None;
            let mut was_active = false;
            loop {
                if scanned.is_none_or(|scanned| scanned.elapsed() >= RESCAN) {
                    let scan = devices.is_none();
                    let paths = devices.clone().unwrap_or_else(keyboard_paths);
                    for path in paths {
                        if keyboards.iter().any(|keyboard| keyboard.path == path) {
                            continue;
                        }
                        let file = open_keyboard(&path).filter(|file| !scan || has_backspace(file));
                        if let Some(file) = file {
                            tracing::debug!(path = %path.display(), "watching for the chord");
                            let chord = Chord::default();
                            keyboards.push(Keyboard { path, file, chord });
                        }
                    }
                    scanned = Some(Instant::now());
                }

                let mut polled: Vec<libc::pollfd> = keyboards
                    .iter()
                    .map(|keyboard| libc::pollfd {
                        fd: keyboard.file.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    })
                    .collect();
                // Safety: the array is valid for its length.
                unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as _, 200) };

                let is_active = active.load(Ordering::Relaxed);
                if is_active && !was_active {
                    keyboards.iter_mut().for_each(|k| k.chord.reset());
                }
                was_active = is_active;

                let mut gone = Vec::new();
                for (keyboard, polled) in keyboards.iter_mut().zip(&polled) {
                    if polled.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
                        && polled.revents & libc::POLLIN == 0
                    {
                        gone.push(keyboard.path.clone());
                        continue;
                    }
                    if polled.revents & libc::POLLIN != 0 && drain(keyboard, is_active) {
                        trigger();
                    }
                }
                keyboards.retain(|keyboard| !gone.contains(&keyboard.path));
            }
        })
        .map(|_| ())
}

/// `struct input_event` of 64-bit Linux.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct InputEvent {
    seconds: i64,
    microseconds: i64,
    kind: u16,
    code: u16,
    value: i32,
}

/// Reads what the device has and returns whether the chord went down.
fn drain(keyboard: &mut Keyboard, active: bool) -> bool {
    let mut events = [InputEvent::default(); 32];
    let size = size_of::<InputEvent>();
    let mut triggered = false;
    loop {
        // Safety: the buffer is valid for that many bytes.
        let read = unsafe {
            libc::read(
                keyboard.file.as_raw_fd(),
                events.as_mut_ptr().cast(),
                size * events.len(),
            )
        };
        if read <= 0 {
            return triggered;
        }
        for event in &events[..read as usize / size] {
            // Value 2 is the autorepeat of a key that is down already.
            if active && event.kind == EV_KEY && event.value != 2 {
                triggered |= keyboard.chord.feed(event.code, event.value == 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;

    fn press(chord: &mut Chord, keys: &[u16]) -> bool {
        keys.iter().fold(false, |_, key| chord.feed(*key, true))
    }

    #[test]
    fn chord_needs_all_three_modifiers() {
        let mut chord = Chord::default();
        assert!(!press(
            &mut chord,
            &[KEY_LEFTCTRL, KEY_LEFTALT, KEY_BACKSPACE]
        ));
        chord.reset();
        assert!(!press(
            &mut chord,
            &[KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_BACKSPACE]
        ));
        chord.reset();
        assert!(!press(
            &mut chord,
            &[KEY_LEFTALT, KEY_LEFTSHIFT, KEY_BACKSPACE]
        ));
        chord.reset();
        assert!(press(
            &mut chord,
            &[KEY_LEFTCTRL, KEY_LEFTALT, KEY_LEFTSHIFT, KEY_BACKSPACE]
        ));
    }

    #[test]
    fn chord_takes_either_side_and_any_order_of_modifiers() {
        let mut chord = Chord::default();
        let keys = [KEY_RIGHTSHIFT, KEY_RIGHTALT, KEY_RIGHTCTRL, KEY_BACKSPACE];
        assert!(press(&mut chord, &keys));
    }

    #[test]
    fn chord_forgets_released_modifiers() {
        let mut chord = Chord::default();
        press(&mut chord, &[KEY_LEFTCTRL, KEY_LEFTALT, KEY_LEFTSHIFT]);
        chord.feed(KEY_LEFTALT, false);
        assert!(!chord.feed(KEY_BACKSPACE, true));
    }

    #[test]
    fn chord_counts_the_second_side_while_the_first_is_released() {
        let mut chord = Chord::default();
        press(
            &mut chord,
            &[KEY_LEFTCTRL, KEY_RIGHTCTRL, KEY_LEFTALT, KEY_LEFTSHIFT],
        );
        chord.feed(KEY_LEFTCTRL, false);
        assert!(chord.feed(KEY_BACKSPACE, true));
    }

    #[test]
    fn release_of_backspace_does_not_trigger() {
        let mut chord = Chord::default();
        press(&mut chord, &[KEY_LEFTCTRL, KEY_LEFTALT, KEY_LEFTSHIFT]);
        assert!(!chord.feed(KEY_BACKSPACE, false));
    }

    /// A fake keyboard: a FIFO taking real `input_event` records.
    struct FakeKeyboard {
        path: PathBuf,
        writer: File,
    }

    impl FakeKeyboard {
        fn new(name: &str) -> Self {
            let path = env::temp_dir().join(format!("wirbel-{name}-{}", std::process::id()));
            let _ = fs::remove_file(&path);
            let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            // Safety: the path is a valid C string.
            assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
            let writer = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            Self { path, writer }
        }

        fn key(&mut self, code: u16, value: i32) {
            let event = InputEvent {
                kind: EV_KEY,
                code,
                value,
                ..InputEvent::default()
            };
            // Safety: reading the bytes of a plain struct.
            let bytes = unsafe {
                std::slice::from_raw_parts((&raw const event).cast::<u8>(), size_of::<InputEvent>())
            };
            self.writer.write_all(bytes).unwrap();
        }

        fn chord(&mut self) {
            for key in [KEY_LEFTCTRL, KEY_LEFTALT, KEY_LEFTSHIFT, KEY_BACKSPACE] {
                self.key(key, 1);
            }
        }

        fn release(&mut self) {
            for key in [KEY_BACKSPACE, KEY_LEFTSHIFT, KEY_LEFTALT, KEY_LEFTCTRL] {
                self.key(key, 0);
            }
        }
    }

    impl Drop for FakeKeyboard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn wait_for(condition: impl Fn() -> bool) -> bool {
        (0..300).any(|_| {
            thread::sleep(Duration::from_millis(10));
            condition()
        })
    }

    #[test]
    fn watcher_fires_from_a_device_without_any_main_loop() {
        let mut keyboard = FakeKeyboard::new("fires");
        let fired = Arc::new(AtomicBool::new(false));
        let (flag, active) = (fired.clone(), Arc::new(AtomicBool::new(true)));
        watch_keyboards(Some(vec![keyboard.path.clone()]), active, move || {
            flag.store(true, Ordering::SeqCst);
        })
        .unwrap();
        keyboard.key(KEY_LEFTCTRL, 1);
        keyboard.key(KEY_BACKSPACE, 1);
        keyboard.key(KEY_LEFTCTRL, 0);
        keyboard.key(KEY_BACKSPACE, 0);
        keyboard.chord();
        assert!(wait_for(|| fired.load(Ordering::SeqCst)));
    }

    #[test]
    fn watcher_ignores_a_session_on_another_vt() {
        let mut keyboard = FakeKeyboard::new("inactive");
        let fired = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let devices = Some(vec![keyboard.path.clone()]);
        watch_keyboards(devices, active.clone(), move || {
            flag.store(true, Ordering::SeqCst);
        })
        .unwrap();
        keyboard.chord();
        thread::sleep(Duration::from_millis(500));
        assert!(!fired.load(Ordering::SeqCst));

        // Back on our VT. What was held on the other one does not count, a new chord does.
        keyboard.release();
        thread::sleep(Duration::from_millis(300));
        active.store(true, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(300));
        keyboard.key(KEY_BACKSPACE, 1);
        keyboard.key(KEY_BACKSPACE, 0);
        thread::sleep(Duration::from_millis(300));
        assert!(!fired.load(Ordering::SeqCst));
        keyboard.chord();
        assert!(wait_for(|| fired.load(Ordering::SeqCst)));
    }

    #[test]
    fn autorepeat_does_not_complete_the_chord() {
        let mut keyboard = FakeKeyboard::new("repeat");
        let fired = Arc::new(AtomicBool::new(false));
        let (flag, active) = (fired.clone(), Arc::new(AtomicBool::new(true)));
        watch_keyboards(Some(vec![keyboard.path.clone()]), active, move || {
            flag.store(true, Ordering::SeqCst);
        })
        .unwrap();
        keyboard.key(KEY_BACKSPACE, 1);
        for key in [KEY_LEFTCTRL, KEY_LEFTALT, KEY_LEFTSHIFT] {
            keyboard.key(key, 1);
        }
        keyboard.key(KEY_BACKSPACE, 2);
        thread::sleep(Duration::from_millis(500));
        assert!(!fired.load(Ordering::SeqCst));
    }

    #[test]
    fn watchdog_stops_the_loop_at_the_deadline() {
        use smithay::reexports::calloop::EventLoop;

        let mut event_loop = EventLoop::<()>::try_new().unwrap();
        let started = Instant::now();
        let _watchdog = Watchdog::start(event_loop.get_signal(), Some(Duration::from_millis(300)));
        event_loop.run(None, &mut (), |_| {}).unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
        assert!(elapsed < GRACE, "{elapsed:?}");
    }

    #[test]
    fn devnull_is_no_terminal() {
        let file = File::open("/dev/null").unwrap();
        assert!(!is_virtual_terminal(file.as_raw_fd()));
    }
}
