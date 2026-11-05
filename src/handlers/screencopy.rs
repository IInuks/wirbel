//! Screen capture for grim, flameshot and, through xdg-desktop-portal-wlr, screen sharing, by two
//! protocols. `wlr-screencopy` is what the tools know. `ext-image-copy-capture` is its successor
//! and not optional: xdg-desktop-portal-wlr 0.8.4 prefers it, and on `wlr-screencopy` its stream
//! stops after the first frame, because only its ext code path drives PipeWire on.
//!
//! A capture never looks at what went to the screen. A client on the primary plane is not in any
//! frame of ours, the hardware cursor neither, and on another GPU the frame is out of reach. So
//! `Capturer` draws the scene once more into a texture of its own, which also answers whether a
//! client wants the cursor. The texture lives on, and a damage tracker draws only what changed.
//!
//! Requests wait in `ScreencopyState` until the backend draws the output and calls
//! `Capturer::run` with its renderer. Plain `copy` asks for a frame, `copy_with_damage` and the
//! ext frames wait for a change since the last time that client looked. Only shm for now: the
//! frame is read back with the GPU, see `docs/PLAN.md` for what that costs.

use std::{
    collections::HashMap,
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, ImportAll, ImportMem, Offscreen, Renderer, Texture,
            damage::OutputDamageTracker,
            element::{Element, Kind},
        },
    },
    output::{Output, WeakOutput},
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, Flags, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::ClientId,
            protocol::{wl_buffer::WlBuffer, wl_shm},
        },
    },
    utils::{Buffer, IsAlive, Logical, Physical, Rectangle, Size, Transform},
    wayland::{
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
            OutputCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, CaptureFailureReason, Frame, FrameRef, ImageCopyCaptureHandler,
            ImageCopyCaptureState, Session, SessionRef,
        },
        shm::{with_buffer_contents, with_buffer_contents_mut},
    },
};

use crate::{Wirbel, render::SceneElement};

/// Version 3 only adds the events that close the list of buffer formats.
const VERSION: u32 = 3;
/// Read back as ARGB, the alpha of the screen means nothing.
const SHM_FORMAT: wl_shm::Format = wl_shm::Format::Xrgb8888;
const BYTES_PER_PIXEL: i32 = 4;

/// The frames a client has seen, per output and cursor, for `copy_with_damage`.
type Seen = Arc<Mutex<HashMap<(String, bool), u64>>>;

#[derive(Clone)]
struct Source {
    output: Output,
    /// In the pixels of the output before its transform.
    region: Rectangle<i32, Buffer>,
    cursor: bool,
}

pub struct FrameData {
    /// `None` for an output that is gone.
    source: Option<Source>,
    used: AtomicBool,
    seen: Seen,
}

/// Whom to tell about a finished capture.
enum Reply {
    Wlr {
        frame: ZwlrScreencopyFrameV1,
        buffer: WlBuffer,
    },
    Ext {
        frame: Frame,
        session: SessionRef,
    },
}

struct Capture {
    reply: Reply,
    source: Source,
    wait_for_change: bool,
    seen: Seen,
}

impl Capture {
    fn is_alive(&self) -> bool {
        match &self.reply {
            Reply::Wlr { frame, .. } => frame.is_alive(),
            Reply::Ext { session, .. } => session.alive(),
        }
    }

    fn buffer(&self) -> WlBuffer {
        match &self.reply {
            Reply::Wlr { buffer, .. } => buffer.clone(),
            Reply::Ext { frame, .. } => frame.buffer(),
        }
    }

    fn fail(self) {
        match self.reply {
            Reply::Wlr { frame, .. } => frame.failed(),
            Reply::Ext { frame, .. } => frame.fail(CaptureFailureReason::Unknown),
        }
    }

    fn finish(self, time: Duration) {
        let size = self.source.region.size;
        match self.reply {
            Reply::Wlr { frame, .. } => {
                frame.flags(Flags::empty());
                if self.wait_for_change {
                    frame.damage(0, 0, size.w as u32, size.h as u32);
                }
                let seconds = time.as_secs();
                frame.ready((seconds >> 32) as u32, seconds as u32, time.subsec_nanos());
            }
            Reply::Ext { frame, .. } => {
                let transform = self.source.output.current_transform();
                frame.success(transform, vec![self.source.region], time);
            }
        }
    }
}

/// The captures that wait for a frame.
pub struct ScreencopyState {
    queue: Vec<Capture>,
    /// Dropping one tells its client that it is over.
    sessions: Vec<Session>,
    copy_capture: ImageCopyCaptureState,
    output_source: OutputCaptureSourceState,
}

impl ScreencopyState {
    pub fn new(display: &DisplayHandle) -> Self {
        display.create_global::<Wirbel, ZwlrScreencopyManagerV1, ()>(VERSION, ());
        Self {
            queue: Vec::new(),
            sessions: Vec::new(),
            copy_capture: ImageCopyCaptureState::new::<Wirbel>(display),
            output_source: OutputCaptureSourceState::new::<Wirbel>(display),
        }
    }

    /// Those that wait for `output` fail, there will not be another frame.
    pub fn output_removed(&mut self, output: &Output) {
        let queue = std::mem::take(&mut self.queue);
        let (gone, queue) = queue
            .into_iter()
            .partition(|capture| capture.source.output == *output);
        self.queue = queue;
        gone.into_iter().for_each(Capture::fail);
        self.sessions
            .retain(|session| output_of_source(&session.source()).as_ref() != Some(output));
    }

    /// Clients learn what buffers it takes now. What waits has the wrong size, it fails.
    pub fn output_changed(&mut self, output: &Output) {
        let queue = std::mem::take(&mut self.queue);
        let (stale, queue): (Vec<_>, Vec<_>) = queue.into_iter().partition(|capture| {
            capture.source.output == *output
                && buffer_region(output, None) != Some(capture.source.region)
                && matches!(capture.reply, Reply::Ext { .. })
        });
        self.queue = queue;
        stale.into_iter().for_each(Capture::fail);
        for session in &self.sessions {
            if output_of_source(&session.source()).as_ref() == Some(output)
                && let Some(constraints) = constraints_of(output)
            {
                session.update_constraints(constraints);
            }
        }
    }
}

fn output_of_source(source: &ImageCaptureSource) -> Option<Output> {
    source.user_data().get::<WeakOutput>()?.upgrade()
}

fn constraints_of(output: &Output) -> Option<BufferConstraints> {
    let mode = output.current_mode()?;
    Some(BufferConstraints {
        size: (mode.size.w, mode.size.h).into(),
        shm: vec![SHM_FORMAT],
        dma: None,
    })
}

/// The whole of `output` or a part of it, in the pixels of its buffers. Empty if nothing is left
/// of the part.
fn buffer_region(
    output: &Output,
    region: Option<Rectangle<i32, Logical>>,
) -> Option<Rectangle<i32, Buffer>> {
    let mode = output.current_mode()?;
    let whole = Size::<i32, Buffer>::from((mode.size.w, mode.size.h));
    let Some(region) = region else {
        return Some(Rectangle::from_size(whole));
    };
    let scale = output.current_scale().fractional_scale();
    let transform = output.current_transform();
    let logical = transform
        .transform_size(mode.size)
        .to_f64()
        .to_logical(scale);
    let bounds = Rectangle::from_size(logical.to_i32_round::<i32>());
    let region = region.intersection(bounds)?;
    let region = region.to_f64().to_buffer(scale, transform, &logical);
    let region = region.to_i32_round();
    (!region.is_empty()).then_some(region)
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for Wirbel {
    fn bind(
        _state: &mut Self,
        _display: &DisplayHandle,
        _client: &Client,
        manager: New<ZwlrScreencopyManagerV1>,
        _data: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(manager, Seen::default());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, Seen> for Wirbel {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        seen: &Seen,
        _display: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_screencopy_manager_v1::Request;
        let (frame, overlay_cursor, output, region) = match request {
            Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => (frame, overlay_cursor, output, None),
            Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                let region = Rectangle::new((x, y).into(), (width, height).into());
                (frame, overlay_cursor, output, Some(region))
            }
            Request::Destroy => return,
            _ => return,
        };
        let source = Output::from_resource(&output).and_then(|output| {
            let region = buffer_region(&output, region)?;
            Some(Source {
                output,
                region,
                cursor: overlay_cursor != 0,
            })
        });
        let frame = init.init(
            frame,
            FrameData {
                source: source.clone(),
                used: AtomicBool::new(false),
                seen: seen.clone(),
            },
        );
        let Some(source) = source else {
            frame.failed();
            return;
        };
        let size = source.region.size;
        let stride = size.w * BYTES_PER_PIXEL;
        frame.buffer(SHM_FORMAT, size.w as u32, size.h as u32, stride as u32);
        if frame.version() >= 3 {
            frame.buffer_done();
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameData> for Wirbel {
    fn request(
        state: &mut Self,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        _display: &DisplayHandle,
        _init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_screencopy_frame_v1::{Error, Request};
        let (buffer, with_damage) = match request {
            Request::Copy { buffer } => (buffer, false),
            Request::CopyWithDamage { buffer } => (buffer, true),
            Request::Destroy => {
                state.screencopy.forget(frame);
                return;
            }
            _ => return,
        };
        if data.used.swap(true, Ordering::Relaxed) {
            frame.post_error(Error::AlreadyUsed, "this frame was already copied");
            return;
        }
        let Some(source) = data.source.clone() else {
            frame.failed();
            return;
        };
        let size = source.region.size;
        let fits = with_buffer_contents(&buffer, |_, _, buffer| {
            buffer.format == SHM_FORMAT
                && buffer.width == size.w
                && buffer.height == size.h
                && buffer.stride >= size.w * BYTES_PER_PIXEL
        });
        if !matches!(fits, Ok(true)) {
            frame.post_error(Error::InvalidBuffer, "the buffer is not the one announced");
            return;
        }
        state.screencopy.queue.push(Capture {
            reply: Reply::Wlr {
                frame: frame.clone(),
                buffer,
            },
            source,
            wait_for_change: with_damage,
            seen: data.seen.clone(),
        });
        // Whether anything changed since the last look comes out of the next frame.
        state.needs_redraw = true;
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        frame: &ZwlrScreencopyFrameV1,
        _data: &FrameData,
    ) {
        state.screencopy.forget(frame);
    }
}

impl ScreencopyState {
    fn forget(&mut self, frame: &ZwlrScreencopyFrameV1) {
        self.queue.retain(
            |capture| !matches!(&capture.reply, Reply::Wlr { frame: other, .. } if other == frame),
        );
    }
}

impl ImageCaptureSourceHandler for Wirbel {}

impl OutputCaptureSourceHandler for Wirbel {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.screencopy.output_source
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl ImageCopyCaptureHandler for Wirbel {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.screencopy.copy_capture
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        constraints_of(&output_of_source(source)?)
    }

    fn new_session(&mut self, session: Session) {
        session.user_data().insert_if_missing(Seen::default);
        self.screencopy.sessions.push(session);
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        let output = output_of_source(&session.source());
        let region = output
            .as_ref()
            .and_then(|output| buffer_region(output, None));
        let (Some(output), Some(region), Some(seen)) =
            (output, region, session.user_data().get::<Seen>().cloned())
        else {
            frame.fail(CaptureFailureReason::Stopped);
            return;
        };
        self.screencopy.queue.push(Capture {
            reply: Reply::Ext {
                frame,
                session: session.clone(),
            },
            source: Source {
                output,
                region,
                cursor: session.draw_cursor(),
            },
            wait_for_change: true,
            seen,
        });
        self.needs_redraw = true;
    }

    fn frame_aborted(&mut self, frame: FrameRef) {
        self.screencopy.queue.retain(
            |capture| !matches!(&capture.reply, Reply::Ext { frame: other, .. } if *other == frame),
        );
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.screencopy.sessions.retain(|other| **other != session);
    }
}

/// One way of looking at an output, with or without the cursor.
struct Target<T> {
    output: String,
    cursor: bool,
    /// Counts the frames with a change in them.
    generation: u64,
    canvas: Option<Canvas<T>>,
}

struct Canvas<T> {
    image: T,
    tracker: OutputDamageTracker,
    made_for: (Size<i32, Physical>, f64, Transform),
    /// Whether `image` holds a frame already.
    drawn: bool,
}

/// What draws the captures, kept by the backend because it depends on the kind of renderer.
pub struct Capturer<T> {
    targets: Vec<Target<T>>,
}

impl<T> Default for Capturer<T> {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
        }
    }
}

impl<T> Capturer<T> {
    fn target(&mut self, output: &Output, cursor: bool) -> &mut Target<T> {
        let name = output.name();
        let index = self
            .targets
            .iter()
            .position(|target| target.output == name && target.cursor == cursor);
        let index = index.unwrap_or_else(|| {
            self.targets.push(Target {
                output: name,
                cursor,
                generation: 0,
                canvas: None,
            });
            self.targets.len() - 1
        });
        &mut self.targets[index]
    }

    /// Answers what waits for `output`, as far as it can be answered now. Called by the backend
    /// after every frame it drew for `output`.
    pub fn run<R>(&mut self, state: &mut Wirbel, renderer: &mut R, output: &Output)
    where
        R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<T> + Bind<T>,
        R::TextureId: Clone + Texture + Send + 'static,
        R::Error: 'static,
    {
        for cursor in [false, true] {
            let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut state.screencopy.queue)
                .into_iter()
                .filter(Capture::is_alive)
                .partition(|capture| {
                    capture.source.output == *output && capture.source.cursor == cursor
                });
            state.screencopy.queue = rest;
            if due.is_empty() {
                continue;
            }
            let waiting = self.capture(state, renderer, output, cursor, due);
            state.screencopy.queue.extend(waiting);
        }
    }

    fn capture<R>(
        &mut self,
        state: &Wirbel,
        renderer: &mut R,
        output: &Output,
        cursor: bool,
        due: Vec<Capture>,
    ) -> Vec<Capture>
    where
        R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<T> + Bind<T>,
        R::TextureId: Clone + Texture + Send + 'static,
        R::Error: 'static,
    {
        let target = self.target(output, cursor);
        let Some(mode) = output.current_mode() else {
            due.into_iter().for_each(Capture::fail);
            return Vec::new();
        };
        let made_for = (
            mode.size,
            output.current_scale().fractional_scale(),
            output.current_transform(),
        );
        if target
            .canvas
            .as_ref()
            .is_none_or(|canvas| canvas.made_for != made_for)
        {
            let size = Size::<i32, Buffer>::from((mode.size.w, mode.size.h));
            target.canvas = match renderer.create_buffer(Fourcc::Abgr8888, size) {
                Ok(image) => Some(Canvas {
                    image,
                    tracker: OutputDamageTracker::from_output(output),
                    made_for,
                    drawn: false,
                }),
                Err(err) => {
                    tracing::warn!(?err, "no room for a screen capture");
                    None
                }
            };
        }
        let Some(canvas) = target.canvas.as_mut() else {
            due.into_iter().for_each(Capture::fail);
            return Vec::new();
        };

        let mut elements: Vec<SceneElement<R>> = state.scene_elements(renderer, output);
        if !cursor {
            elements.retain(|element| element.kind() != Kind::Cursor);
        }
        let age = usize::from(canvas.drawn);
        let mut framebuffer = match renderer.bind(&mut canvas.image) {
            Ok(framebuffer) => framebuffer,
            Err(err) => {
                tracing::warn!(?err, "failed to bind a screen capture");
                due.into_iter().for_each(Capture::fail);
                return Vec::new();
            }
        };
        let color = state.config.background_color;
        let rendered =
            canvas
                .tracker
                .render_output(renderer, &mut framebuffer, age, &elements, color);
        match rendered {
            Ok(result) => {
                canvas.drawn = true;
                if result.damage.is_some_and(|damage| !damage.is_empty()) {
                    target.generation += 1;
                }
            }
            Err(err) => {
                tracing::warn!(?err, "failed to render a screen capture");
                canvas.drawn = false;
                due.into_iter().for_each(Capture::fail);
                return Vec::new();
            }
        }

        let generation = target.generation;
        let key = (output.name(), cursor);
        let mut waiting = Vec::new();
        for capture in due {
            let seen = capture.seen.lock().unwrap().insert(key.clone(), generation);
            if capture.wait_for_change && seen.is_some_and(|seen| seen >= generation) {
                waiting.push(capture);
                continue;
            }
            match copy_to_client(renderer, &framebuffer, &capture) {
                Ok(()) => capture.finish(state.clock.now().into()),
                Err(err) => {
                    tracing::warn!(%err, "failed to hand out a screen capture");
                    capture.fail();
                }
            }
        }
        waiting
    }
}

/// Reads `framebuffer` back, the part this capture is about. BGRA is an extension of GLES that
/// every driver here has, but this is no place to fail over one.
fn copy_to_client<R>(
    renderer: &mut R,
    framebuffer: &R::Framebuffer<'_>,
    capture: &Capture,
) -> Result<(), Box<dyn std::error::Error>>
where
    R: ExportMem,
    R::Error: 'static,
{
    let region = capture.source.region;
    let (mapping, swap_red_blue) =
        match renderer.copy_framebuffer(framebuffer, region, Fourcc::Argb8888) {
            Ok(mapping) => (mapping, false),
            Err(_) => (
                renderer.copy_framebuffer(framebuffer, region, Fourcc::Abgr8888)?,
                true,
            ),
        };
    let pixels = renderer.map_texture(&mapping)?;

    let row = region.size.w as usize * BYTES_PER_PIXEL as usize;
    let rows = region.size.h as usize;
    if pixels.len() < row * rows {
        return Err("too few pixels came back".into());
    }
    let target = capture.buffer();
    let written = with_buffer_contents_mut(&target, |memory, length, buffer| {
        let (offset, stride) = (buffer.offset as usize, buffer.stride as usize);
        if buffer.width != region.size.w
            || buffer.height != region.size.h
            || stride < row
            || offset + stride * rows > length
        {
            return false;
        }
        for (y, source) in pixels.chunks_exact(row).take(rows).enumerate() {
            // Safety: within the pool as of the check above, and the access goes by the rules of
            // `with_buffer_contents_mut`: through a pointer, and a client that shrinks the pool
            // under it is caught there.
            unsafe {
                let target = memory.add(offset + y * stride);
                ptr::copy_nonoverlapping(source.as_ptr(), target, row);
                if swap_red_blue {
                    for pixel in 0..region.size.w as usize {
                        ptr::swap(target.add(pixel * 4), target.add(pixel * 4 + 2));
                    }
                }
            }
        }
        true
    })?;
    if !written {
        return Err("the buffer does not fit".into());
    }
    Ok(())
}
