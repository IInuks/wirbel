//! The clipboard of a shadow, read and set from outside. It is the shadow's own: the clipboard
//! of the desktop is another one and stays as it is.
//!
//! - `shadow clip get` TAB PATH writes the text in the clipboard to that file and answers
//!   `ok BYTES`, or an error if the clipboard is empty or holds no text.
//! - `shadow clip set` TAB PATH puts the content of that file in the clipboard, as if a program
//!   had copied it.
//!
//! Files, because a clipboard may hold more than a line of the socket takes.

use std::{
    fs,
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use smithay::wayland::selection::{
    SelectionTarget,
    data_device::{
        current_data_device_selection_userdata, request_data_device_client_selection,
        set_data_device_selection,
    },
};

use crate::Wirbel;

/// What the compositor itself put in a clipboard: `None` stands for the X server, whose
/// selection it passes on, `Some` is text set by `shadow clip set`.
pub type Held = Option<Arc<Vec<u8>>>;

/// The names text goes by, the best first.
const TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];
/// More than this is no text anybody types.
const MAX_BYTES: usize = 16 * 1024 * 1024;
/// How long a program may take to hand over what it copied.
const PATIENCE: Duration = Duration::from_secs(5);

/// Writes `data` to `fd` without holding up the compositor: the reader may be slow.
pub fn write_out(fd: OwnedFd, data: Arc<Vec<u8>>) {
    std::thread::spawn(move || {
        let mut file = fs::File::from(fd);
        let _ = file.write_all(&data);
    });
}

/// Reads `fd` to its end, giving up after `PATIENCE` without news.
fn read_all(fd: OwnedFd) -> Result<Vec<u8>, String> {
    let mut file = fs::File::from(fd);
    let mut data = Vec::new();
    let mut buffer = [0; 64 * 1024];
    let mut last = Instant::now();
    loop {
        let mut poll = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Safety: one valid pollfd.
        let ready = unsafe { libc::poll(&mut poll, 1, 200) };
        if ready == 0 {
            if last.elapsed() > PATIENCE {
                return Err("the program did not hand over its clipboard".into());
            }
            continue;
        }
        match file.read(&mut buffer) {
            Ok(0) => return Ok(data),
            Ok(count) => {
                data.extend_from_slice(&buffer[..count]);
                last = Instant::now();
                if data.len() > MAX_BYTES {
                    return Err("the clipboard holds more than 16 MB".into());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(format!("failed to read the clipboard: {err}")),
        }
    }
}

fn pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let (reader, writer) = std::io::pipe().map_err(|err| format!("no pipe: {err}"))?;
    Ok((reader.into(), writer.into()))
}

impl Wirbel {
    /// Puts text in the clipboard.
    pub fn clip_set(&mut self, text: Vec<u8>) {
        let types = TEXT_TYPES.iter().map(|name| name.to_string()).collect();
        let held: Held = Some(Arc::new(text));
        set_data_device_selection(&self.display_handle, &self.seat, types, held);
        if let Some(shadow) = self.shadow.as_mut() {
            shadow.clipboard += 1;
        }
    }

    /// Writes the text in the clipboard to `path` and answers `peer` when that is done.
    pub fn clip_get(&mut self, peer: u64, path: PathBuf) -> Result<(), String> {
        let held = current_data_device_selection_userdata(&self.seat).map(|held| held.clone());
        let reader = match held {
            // Our own text needs no detour.
            Some(Some(text)) => {
                fs::write(&path, &*text).map_err(|err| format!("{}: {err}", path.display()))?;
                self.ipc.reply(peer, &format!("ok {}", text.len()));
                return Ok(());
            }
            // An X11 program copied it.
            Some(None) => {
                let (reader, writer) = pipe()?;
                let xwm = self.xwm.as_mut().ok_or("the clipboard is empty")?;
                xwm.send_selection(SelectionTarget::Clipboard, TEXT_TYPES[0].into(), writer)
                    .map_err(|err| format!("failed to ask the X11 program: {err}"))?;
                reader
            }
            None => {
                let mut asked = None;
                for name in TEXT_TYPES {
                    let (reader, writer) = pipe()?;
                    use smithay::wayland::selection::data_device::SelectionRequestError as E;
                    match request_data_device_client_selection(&self.seat, name.into(), writer) {
                        Ok(()) => {
                            asked = Some(reader);
                            break;
                        }
                        Err(E::InvalidMimetype) => {}
                        Err(_) => return Err("the clipboard is empty".into()),
                    }
                }
                asked.ok_or("the clipboard holds no text")?
            }
        };
        let replies = self.reply_sender()?;
        std::thread::spawn(move || {
            let answer = read_all(reader).and_then(|data| write_text(&path, &data));
            let answer = match answer {
                Ok(bytes) => format!("ok {bytes}"),
                Err(err) => format!("error {err}"),
            };
            let _ = replies.send((peer, answer));
        });
        Ok(())
    }
}

fn write_text(path: &Path, data: &[u8]) -> Result<usize, String> {
    fs::write(path, data).map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(data.len())
}

/// A `shadow clip ...` request, the part after `clip`. `Ok(None)` answers later.
pub(crate) fn request(state: &mut Wirbel, peer: u64, rest: &str) -> Result<Option<String>, String> {
    let mut fields = rest.trim_start_matches([' ', '\t']).splitn(2, '\t');
    let (command, path) = (fields.next(), fields.next());
    let path = path.filter(|path| path.starts_with('/'));
    match (command, path) {
        (Some("get"), Some(path)) => {
            state.clip_get(peer, path.into())?;
            Ok(None)
        }
        (Some("set"), Some(path)) => {
            let size = fs::metadata(path)
                .map_err(|err| format!("{path}: {err}"))?
                .len();
            if size > MAX_BYTES as u64 {
                return Err("more than 16 MB do not go in the clipboard".into());
            }
            let text = fs::read(path).map_err(|err| format!("{path}: {err}"))?;
            state.clip_set(text);
            Ok(Some("ok".into()))
        }
        _ => Err("expected clip get|set and an absolute path, separated by tabs".into()),
    }
}
