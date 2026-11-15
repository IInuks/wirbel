//! The sound of invisible windows. A program started with `wirbel vwm open` plays into a sink
//! of its own that goes nowhere, so it is silent unless asked: `audio NAME listen on` routes
//! it to the speakers, `audio NAME record FILE` writes it down, `audio NAME level` and the
//! wait conditions `sound` and `silence` say whether it plays.
//!
//! All of it is done with the helper programs of the sound server, `pactl` and `parec`, which
//! PipeWire and PulseAudio both bring: from the command, never from the compositor, which must
//! not wait for a sound server. Without them, or without a sound server, a program simply
//! plays as it would otherwise.

use std::{
    fs,
    io::Read,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use super::manager::state_dir;

/// The name of the sink of an invisible window.
pub fn sink_name(name: &str) -> String {
    format!("wirbel-vwm-{name}")
}

fn pactl(args: &[&str]) -> Result<String, String> {
    let output = Command::new("pactl")
        .args(args)
        .stdin(Stdio::null())
        .output();
    let output = output.map_err(|err| format!("the program `pactl` is needed for sound: {err}"))?;
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pactl {}: {}", args.join(" "), why.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Where the numbers of the modules loaded for a window are kept, to unload them later.
fn module_file(name: &str, what: &str) -> Option<PathBuf> {
    Some(state_dir(name)?.join(format!("audio-{what}")))
}

fn unload(name: &str, what: &str) {
    let Some(file) = module_file(name, what) else {
        return;
    };
    if let Ok(module) = fs::read_to_string(&file) {
        let _ = pactl(&["unload-module", module.trim()]);
        let _ = fs::remove_file(file);
    }
}

/// Makes the sink of a window. Returns its name, for `PULSE_SINK`.
pub fn create_sink(name: &str) -> Result<String, String> {
    let sink = sink_name(name);
    unload(name, "sink");
    let module = pactl(&[
        "load-module",
        "module-null-sink",
        &format!("sink_name={sink}"),
        &format!("sink_properties=device.description=wirbel-vwm-{name}"),
    ])?;
    let file = module_file(name, "sink").ok_or("cannot tell where the state folder is")?;
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    fs::write(&file, &module).map_err(|err| format!("{}: {err}", file.display()))?;
    Ok(sink)
}

/// Takes away what was set up for the sound of a window.
pub fn remove(name: &str) {
    unload(name, "listen");
    unload(name, "sink");
}

/// Removes the sinks of windows that are gone: one that ended by itself could not tidy up.
pub fn sweep(alive: &[String]) {
    let Some(root) = state_dir("x").and_then(|dir| dir.parent().map(PathBuf::from)) else {
        return;
    };
    for entry in fs::read_dir(root).into_iter().flatten().flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !alive.contains(&name) && entry.path().join("audio-sink").exists() {
            remove(&name);
        }
    }
}

fn has_sink(name: &str) -> Result<(), String> {
    match module_file(name, "sink").is_some_and(|file| file.exists()) {
        true => Ok(()),
        false => Err(format!(
            "`{name}` has no sound of its own (started with --sound, or no sound server)"
        )),
    }
}

/// Routes the sound of a window to the speakers, or stops that.
pub fn listen(name: &str, on: bool) -> Result<(), String> {
    has_sink(name)?;
    unload(name, "listen");
    if on {
        let source = format!("source={}.monitor", sink_name(name));
        let module = pactl(&["load-module", "module-loopback", &source, "latency_msec=50"])?;
        let file = module_file(name, "listen").ok_or("cannot tell where the state folder is")?;
        fs::write(&file, module).map_err(|err| format!("{}: {err}", file.display()))?;
    }
    Ok(())
}

/// The rate the level is measured at: enough to tell sound from silence.
const RATE: u32 = 8000;
/// Below this share of full volume it is silence: a null sink gives exact zeros, a little
/// room is for dither.
pub const QUIET: f64 = 0.0005;

/// A running capture of what a window plays, as mono 16 bit samples.
pub struct Listener {
    child: Child,
}

impl Listener {
    pub fn of(name: &str) -> Result<Self, String> {
        has_sink(name)?;
        let device = format!("--device={}.monitor", sink_name(name));
        let child = Command::new("parec")
            .args([
                &device,
                "--raw",
                "--format=s16le",
                "--channels=1",
                "--latency-msec=50",
            ])
            .arg(format!("--rate={RATE}"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("the program `parec` is needed to hear a window: {err}"))?;
        Ok(Self { child })
    }

    /// The loudest level within the next `time`: the root mean square of each tenth of a
    /// second, as a share of full volume, and the highest of them.
    pub fn level(&mut self, time: Duration) -> Result<f64, String> {
        let stdout = self
            .child
            .stdout
            .as_mut()
            .ok_or("the capture has no output")?;
        let wanted = (u128::from(RATE) * 2 * time.as_millis() / 1000) as usize;
        let mut bytes = vec![0; wanted.max(2) & !1];
        let start = Instant::now();
        stdout
            .read_exact(&mut bytes)
            .map_err(|err| format!("the capture ended after {:?}: {err}", start.elapsed()))?;
        Ok(loudest(&bytes))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The highest level of the tenths of a second in raw mono 16 bit samples.
pub fn loudest(bytes: &[u8]) -> f64 {
    let window = (RATE as usize / 10) * 2;
    let level = |chunk: &[u8]| {
        let samples = chunk
            .chunks_exact(2)
            .map(|pair| f64::from(i16::from_le_bytes([pair[0], pair[1]])));
        let (sum, count) = samples.fold((0.0, 0u32), |(sum, count), s| (sum + s * s, count + 1));
        match count {
            0 => 0.0,
            count => (sum / f64::from(count)).sqrt() / 32768.0,
        }
    };
    bytes.chunks(window).map(level).fold(0.0, f64::max)
}

/// The head of a WAV file of 16 bit stereo at `RECORD_RATE` with `bytes` of samples. A length
/// that is not known yet is written as the largest, which players take as "to the end".
pub fn wav_header(bytes: Option<u32>) -> Vec<u8> {
    let data = bytes.unwrap_or(u32::MAX - 36);
    let mut header = Vec::with_capacity(44);
    header.extend(b"RIFF");
    header.extend((data + 36).to_le_bytes());
    header.extend(b"WAVEfmt ");
    header.extend(16u32.to_le_bytes());
    header.extend(1u16.to_le_bytes());
    header.extend(2u16.to_le_bytes());
    header.extend(RECORD_RATE.to_le_bytes());
    header.extend((RECORD_RATE * 4).to_le_bytes());
    header.extend(4u16.to_le_bytes());
    header.extend(16u16.to_le_bytes());
    header.extend(b"data");
    header.extend(data.to_le_bytes());
    header
}

const RECORD_RATE: u32 = 48000;

/// Writes what a window plays to a WAV file, for `time` or until the command is stopped.
pub fn record(name: &str, file: &str, time: Option<Duration>) -> Result<(), String> {
    use std::io::{Seek, SeekFrom, Write};
    has_sink(name)?;
    let device = format!("--device={}.monitor", sink_name(name));
    let mut child = Command::new("parec")
        .args([
            &device,
            "--raw",
            "--format=s16le",
            "--channels=2",
            "--latency-msec=50",
        ])
        .arg(format!("--rate={RECORD_RATE}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("the program `parec` is needed to record: {err}"))?;
    let mut samples = child.stdout.take().ok_or("the capture has no output")?;
    let fail = |err: std::io::Error| format!("{file}: {err}");
    let mut out = fs::File::create(file).map_err(fail)?;
    // The length is filled in at the end; stopped half way, the file still plays.
    out.write_all(&wav_header(None)).map_err(fail)?;
    let wanted = time.map(|time| (u128::from(RECORD_RATE) * 4 * time.as_millis() / 1000) as u64);
    let mut written: u64 = 0;
    let mut buffer = [0; 16 * 1024];
    while wanted.is_none_or(|wanted| written < wanted) {
        let room = wanted.map_or(buffer.len() as u64, |wanted| {
            (wanted - written).min(buffer.len() as u64)
        });
        match samples.read(&mut buffer[..room as usize]) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                out.write_all(&buffer[..count]).map_err(fail)?;
                written += count as u64;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    out.seek(SeekFrom::Start(0)).map_err(fail)?;
    out.write_all(&wav_header(Some(
        written.min(u64::from(u32::MAX - 36)) as u32
    )))
    .map_err(fail)?;
    match written {
        0 => Err("nothing was recorded: the sound server gave no samples".into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(loudest(&[]), 0.0);
        assert_eq!(loudest(&[0; 4000]), 0.0);
        // A square wave at half volume.
        let half: Vec<u8> = (0..1600)
            .flat_map(|n| (if n % 2 == 0 { 16384i16 } else { -16384 }).to_le_bytes())
            .collect();
        assert!((loudest(&half) - 0.5).abs() < 1e-9);
        // Silence with a short burst is as loud as the burst.
        let mut burst = vec![0; 8000];
        burst.extend(&half);
        burst.extend(vec![0; 8000]);
        assert!(loudest(&burst) > 0.4);
        assert!(loudest(&[0, 0, 1, 0, 0, 0]) < QUIET);
        assert_eq!(sink_name("b1"), "wirbel-vwm-b1");
        let header = wav_header(Some(1000));
        assert_eq!(header.len(), 44);
        assert_eq!(&header[..4], b"RIFF");
        assert_eq!(header[4..8], 1036u32.to_le_bytes());
        assert_eq!(header[40..44], 1000u32.to_le_bytes());
        assert_eq!(wav_header(None)[40..44], (u32::MAX - 36).to_le_bytes());
    }
}
