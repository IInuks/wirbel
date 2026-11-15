//! Headless backend: renders on a GPU render node into a buffer nobody sees. No display, no input
//! devices, no seat, no VT. Clients see one virtual output, and screencopy (`grim`) shows what is on
//! it. Made for tests and scripts that must not show up in a running desktop.
//!
//! Frames follow a fixed 60 Hz clock, but only when something asked for one, like the nested
//! backend does with the redraw requests of its window.

use std::{
    cell::RefCell,
    error::Error,
    fs::{self, OpenOptions},
    os::{fd::OwnedFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use smithay::{
    backend::{
        allocator::{Fourcc, gbm::GbmDevice},
        drm::{DrmDeviceFd, DrmNode, NodeType},
        egl::{EGLContext, EGLDisplay},
        renderer::{
            Bind, ImportDma, ImportMemWl, Offscreen,
            damage::OutputDamageTracker,
            gles::{GlesRenderbuffer, GlesRenderer, GlesTexture},
        },
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            EventLoop, LoopHandle,
            timer::{TimeoutAction, Timer},
        },
        wayland_protocols::wp::presentation_time::server::wp_presentation_feedback,
    },
    utils::{DeviceFd, Size, Transform},
    wayland::presentation::Refresh,
};

use crate::{Wirbel, backend::drm::preferred_gpu, cursor::Cursors, handlers::Capturer};

const REFRESH_MHZ: i32 = 60_000;

/// The size of the virtual output when the command line does not give one.
pub const DEFAULT_SIZE: (i32, i32) = (1280, 720);

fn frame_time() -> Duration {
    Duration::from_secs_f64(1_000.0 / f64::from(REFRESH_MHZ))
}

struct Headless {
    renderer: GlesRenderer,
    /// What frames go into. Nobody looks at it: it is there so that rendering works as it does
    /// on a screen, with the same damage and the same states for the feedback.
    buffer: GlesRenderbuffer,
    damage_tracker: OutputDamageTracker,
    capturer: Capturer<GlesTexture>,
    output: Output,
    /// Whether the buffer holds a frame yet. Its age is 1 after that, as there is only one.
    rendered: bool,
    /// Whether a frame is due on the timer.
    scheduled: bool,
    /// When the last frame was drawn, to keep the frames on the refresh clock.
    last_frame: Option<Instant>,
}

/// Parses `WxH`, such as `1280x720`.
pub fn parse_size(text: &str) -> Option<(i32, i32)> {
    let (w, h) = text.split_once('x')?;
    let size = (w.parse().ok()?, h.parse().ok()?);
    (size.0 > 0 && size.1 > 0 && size.0 <= 16384 && size.1 <= 16384).then_some(size)
}

/// The render nodes to try, best first. A configured `render_gpu` is the only one: a test should
/// not end up on another GPU than the user asked for. Otherwise the GPU the DRM backend would
/// take comes first, then any other render node.
fn candidates(configured: Option<&str>) -> Vec<PathBuf> {
    if let Some(name) = configured {
        return preferred_gpu("seat0", Some(name)).into_iter().collect();
    }
    let mut paths: Vec<PathBuf> = preferred_gpu("seat0", None).into_iter().collect();
    let mut others: Vec<PathBuf> = fs::read_dir("/dev/dri")
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
                })
                .collect()
        })
        .unwrap_or_default();
    others.sort();
    paths.extend(others);
    paths
}

/// The render node of a card or render node path. It needs no master and no seat.
fn render_node(path: &Path) -> Result<DrmNode, Box<dyn Error>> {
    let node = DrmNode::from_path(path)?;
    if node.ty() == NodeType::Render {
        return Ok(node);
    }
    Ok(node
        .node_with_type(NodeType::Render)
        .ok_or("the card has no render node")??)
}

fn open_node(node: DrmNode) -> Result<DrmDeviceFd, Box<dyn Error>> {
    let path = node.dev_path().ok_or("no path for the render node")?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)?;
    Ok(DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(file))))
}

fn renderer_on(node: DrmNode) -> Result<GlesRenderer, Box<dyn Error>> {
    let gbm = GbmDevice::new(open_node(node)?)?;
    // Safety: the display keeps the GBM device alive, and the renderer stays on this thread.
    let display = unsafe { EGLDisplay::new(gbm)? };
    let context = EGLContext::new(&display)?;
    // Safety: the context is only made current on this thread.
    Ok(unsafe { GlesRenderer::new(context)? })
}

/// Finds a render node that EGL can draw on.
fn open_renderer(configured: Option<&str>) -> Result<(DrmNode, GlesRenderer), Box<dyn Error>> {
    let paths = candidates(configured);
    if paths.is_empty() {
        return Err("no render node in /dev/dri".into());
    }
    let mut last = None;
    for path in paths {
        match render_node(&path).and_then(|node| Ok((node, renderer_on(node)?))) {
            Ok(found) => return Ok(found),
            Err(err) => {
                tracing::warn!(%err, ?path, "cannot render on this GPU");
                last = Some(err);
            }
        }
    }
    Err(last.unwrap_or_else(|| "no GPU can render".into()))
}

pub fn init(
    event_loop: &mut EventLoop<'static, Wirbel>,
    state: &mut Wirbel,
    size: (i32, i32),
) -> Result<(), Box<dyn Error>> {
    let (node, mut renderer) = open_renderer(state.config.render_gpu.as_deref())?;
    tracing::info!(%node, "headless, rendering on");

    state.shm_state.update_formats(renderer.shm_formats());
    let formats = renderer.dmabuf_formats();
    state.offer_dmabufs(node.dev_id(), formats);
    match open_node(node) {
        Ok(fd) => state.offer_syncobj(fd),
        Err(err) => tracing::warn!(%err, "no explicit sync"),
    }

    let buffer = Offscreen::<GlesRenderbuffer>::create_buffer(
        &mut renderer,
        Fourcc::Abgr8888,
        Size::from(size),
    )
    .map_err(|err| format!("no offscreen buffer: {err}"))?;

    let mode = Mode {
        size: size.into(),
        refresh: REFRESH_MHZ,
    };
    let output = Output::new(
        "headless".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "wirbel".into(),
            model: "headless".into(),
            serial_number: "0".into(),
        },
    );
    let _global = output.create_global::<Wirbel>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.add_output(&output);

    state.cursors = Some(Cursors::load(
        state.config.cursor_theme.as_deref(),
        state.config.cursor_size,
    ));

    let headless = Rc::new(RefCell::new(Headless {
        damage_tracker: OutputDamageTracker::from_output(&output),
        capturer: Capturer::default(),
        renderer,
        buffer,
        output,
        rendered: false,
        scheduled: false,
        last_frame: None,
    }));

    state.redraw = {
        let headless = headless.clone();
        let handle = event_loop.handle();
        Box::new(move || schedule(&headless, &handle))
    };
    state.resize_output = Some({
        let headless = headless.clone();
        Box::new(move |size| headless.borrow_mut().resize(size))
    });
    state.import_dmabuf = {
        let headless = headless.clone();
        Box::new(move |dmabuf| {
            let result = headless.borrow_mut().renderer.import_dmabuf(dmabuf, None);
            if let Err(err) = &result {
                tracing::warn!(%err, "failed to import a dmabuf");
            }
            result.is_ok()
        })
    };
    // The first frame, so that screencopy has something before any client draws.
    schedule(&headless, &event_loop.handle());
    Ok(())
}

/// Puts a frame on the timer, at the next tick of the refresh clock.
fn schedule(headless: &Rc<RefCell<Headless>>, handle: &LoopHandle<'static, Wirbel>) {
    let delay = {
        let mut this = headless.borrow_mut();
        if this.scheduled {
            return;
        }
        this.scheduled = true;
        let next = this.last_frame.map(|last| last + frame_time());
        next.map_or(Duration::ZERO, |next| {
            next.saturating_duration_since(Instant::now())
        })
    };
    let frame = headless.clone();
    let timer = handle.insert_source(Timer::from_duration(delay), move |_, _, state| {
        let mut this = frame.borrow_mut();
        this.scheduled = false;
        this.render(state);
        TimeoutAction::Drop
    });
    if let Err(err) = timer {
        tracing::error!(%err, "failed to schedule a frame");
        headless.borrow_mut().scheduled = false;
    }
}

impl Headless {
    /// Gives the output and the buffer behind it another size.
    fn resize(&mut self, size: (i32, i32)) -> Result<Output, String> {
        if size.0 < 1 || size.1 < 1 {
            return Err("the size must be at least 1x1".into());
        }
        self.buffer = Offscreen::<GlesRenderbuffer>::create_buffer(
            &mut self.renderer,
            Fourcc::Abgr8888,
            Size::from(size),
        )
        .map_err(|err| format!("no offscreen buffer: {err}"))?;
        let mode = Mode {
            size: size.into(),
            refresh: REFRESH_MHZ,
        };
        if let Some(old) = self.output.current_mode() {
            self.output.delete_mode(old);
        }
        self.output
            .change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
        self.damage_tracker = OutputDamageTracker::from_output(&self.output);
        self.rendered = false;
        Ok(self.output.clone())
    }

    fn render(&mut self, state: &mut Wirbel) {
        let start = state.stats.start();
        self.last_frame = Some(Instant::now());
        let age = usize::from(self.rendered);
        let elements = state.output_elements(&mut self.renderer, &self.output);
        let result = match self.renderer.bind(&mut self.buffer) {
            Ok(mut framebuffer) => self
                .damage_tracker
                .render_output(
                    &mut self.renderer,
                    &mut framebuffer,
                    age,
                    &elements,
                    state.config.background_color,
                )
                .map(|result| {
                    let damaged = result.damage.is_some_and(|damage| !damage.is_empty());
                    (damaged, result.states)
                })
                .map_err(|err| format!("{err:?}")),
            Err(err) => Err(err.to_string()),
        };
        drop(elements);
        let states = match result {
            Ok((damaged, states)) => {
                if let Some(shadow) = state.shadow.as_mut().filter(|_| damaged) {
                    shadow.damaged();
                }
                states
            }
            Err(err) => {
                tracing::error!(err, "failed to render");
                return;
            }
        };
        self.rendered = true;

        state.update_scanout_outputs(&self.output, &states);
        let mut feedback = state.presentation_feedback(&self.output, &states);
        // Nothing waits for a vblank here: the frame is done when the GPU is.
        feedback.presented(
            state.clock.now(),
            Refresh::fixed(frame_time()),
            0,
            wp_presentation_feedback::Kind::empty(),
        );
        state.send_frame_callbacks(&self.output);
        self.capturer.run_with(
            state,
            &mut self.renderer,
            &self.output,
            Wirbel::output_elements,
        );
        crate::vwm::shot::take(state, &mut self.renderer, &self.output);
        state.stats.rendered(start);
        state.stats.queued();
        state.stats.vblank(frame_time(), true, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("1280x720"), Some((1280, 720)));
        assert_eq!(parse_size("640x480"), Some((640, 480)));
        assert_eq!(parse_size("0x480"), None);
        assert_eq!(parse_size("1280"), None);
        assert_eq!(parse_size("axb"), None);
        assert_eq!(parse_size("-5x5"), None);
        assert_eq!(parse_size("99999x5"), None);
    }

    #[test]
    fn a_configured_gpu_is_the_only_candidate() {
        assert_eq!(
            candidates(Some("renderD128")),
            [PathBuf::from("/dev/dri/renderD128")]
        );
    }
}
