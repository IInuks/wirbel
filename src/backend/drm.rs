//! The real thing: DRM/KMS on all GPUs of the seat, libinput, and libseat for the session, which
//! elogind provides here. Runs from a virtual terminal, see `docs/TTY-TEST.md`.
//!
//! One GPU renders (`render_gpu`, by default the boot GPU, like wlroots does), the others get the
//! frames copied over by `MultiRenderer`. Each output draws on its own, paced by `Pacing`. All
//! state is in one `Drm` behind an `Rc`, which the callbacks of the event loop share; none of them
//! holds the borrow while calling into `Wirbel`.

use std::{
    cell::RefCell,
    collections::HashMap,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::Ordering,
    time::Duration,
};

use smithay::{
    backend::{
        SwapBuffersError,
        allocator::{
            Fourcc, Modifier,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmAccessError, DrmDevice, DrmDeviceFd, DrmError, DrmEvent, DrmEventMetadata,
            DrmEventTime, DrmNode, Planes,
            compositor::{FrameFlags, PrimaryPlaneElement, RenderFrameError},
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLDevice, EGLDisplay, context::ContextPriority},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            ImportDma, ImportMemWl,
            damage::Error as DamageError,
            element::RenderElementStates,
            gles::{GlesRenderer, GlesTexture},
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{
            Event as SessionEvent, Session,
            libseat::{LibSeatSession, LibSeatSessionNotifier},
        },
        udev::{UdevBackend, UdevEvent, primary_gpu},
    },
    desktop::utils::OutputPresentationFeedback,
    output::{Mode, Output, PhysicalProperties, Scale},
    reexports::{
        calloop::{
            EventLoop, LoopHandle, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::{
            Device as _,
            control::{Mode as DrmMode, ModeFlags, ModeTypeFlags, connector, crtc},
        },
        input::{self, DeviceCapability, Libinput},
        rustix::fs::OFlags,
        wayland_protocols::wp::{
            linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1::TrancheFlags,
            presentation_time::server::wp_presentation_feedback,
        },
        wayland_server::backend::GlobalId,
    },
    utils::{DeviceFd, Monotonic, Time},
    wayland::{
        dmabuf::{DmabufFeedback, DmabufFeedbackBuilder},
        presentation::Refresh,
    },
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::{
    Wirbel,
    backend::pacing::{Pacing, Presented},
    cursor::Cursors,
    handlers::Capturer,
    monitor::{self, ModeInfo},
    render::SceneElement,
    safety::SessionActive,
};

/// Plain 8 bits per channel. Nothing here deals with more, and every driver has these.
const FORMATS: &[Fourcc] = &[Fourcc::Argb8888, Fourcc::Abgr8888];

/// The share of a frame that clients get to draw after a vblank, before we render.
const CLIENT_SHARE: f64 = 0.6;

type Api = GbmGlesBackend<GlesRenderer, DrmDeviceFd>;
type Renderer<'a> = MultiRenderer<'a, 'a, Api, Api>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type Allocator = GbmAllocator<DrmDeviceFd>;

struct Surface {
    output: Output,
    global: Option<GlobalId>,
    drm_output: DrmOutput<Allocator, Exporter, Option<OutputPresentationFeedback>, DrmDeviceFd>,
    pacing: Pacing,
    /// Whether the log already told that this output shows something.
    presented: bool,
    /// The dmabuf feedback of this output, if it could be put together.
    feedback: Option<OutputFeedback>,
    /// What the last frame found out about the surfaces on it.
    states: Rc<RenderElementStates>,
    /// Whether a client buffer is on the primary plane instead of a frame of ours.
    scanout: bool,
}

/// What clients should allocate: for a surface we draw, and for one that could go to the screen as
/// it is. Only the latter is worth a format the renderer finds harder.
#[derive(Clone)]
struct OutputFeedback {
    render: DmabufFeedback,
    scanout: DmabufFeedback,
}

struct Device {
    manager: DrmOutputManager<Allocator, Exporter, Option<OutputPresentationFeedback>, DrmDeviceFd>,
    scanner: DrmScanner,
    /// `None` for a GPU that cannot render here: it only scans out what another one drew.
    render_node: Option<DrmNode>,
    surfaces: HashMap<crtc::Handle, Surface>,
    token: RegistrationToken,
}

struct Drm {
    session: LibSeatSession,
    gpus: GpuManager<Api>,
    /// Set by the first GPU that manages to render.
    render_gpu: Option<DrmNode>,
    devices: HashMap<DrmNode, Device>,
    /// Always on the render GPU, whichever GPU the output hangs on.
    capturer: Capturer<GlesTexture>,
    keyboards: Vec<input::Device>,
    handle: LoopHandle<'static, Wirbel>,
}

/// The name users know a connector by, the same as in `/sys/class/drm` and Mango.
pub fn connector_name(connector: &connector::Info) -> String {
    let kind = connector.interface().as_str();
    format!("{kind}-{}", connector.interface_id())
}

pub fn mode_info(mode: &DrmMode) -> ModeInfo {
    let (width, height) = mode.size();
    ModeInfo {
        width: width.into(),
        height: height.into(),
        refresh_mhz: Mode::from(*mode).refresh,
        preferred: mode.mode_type().contains(ModeTypeFlags::PREFERRED),
        interlaced: mode.flags().contains(ModeFlags::INTERLACE),
    }
}

/// The card to prefer for rendering: as configured, or else the boot GPU.
pub fn preferred_gpu(seat: &str, configured: Option<&str>) -> Option<PathBuf> {
    match configured {
        Some(name) => Some(Path::new("/dev/dri").join(name)),
        None => primary_gpu(seat).ok().flatten(),
    }
}

/// The monitor behind a connector, from what the kernel keeps in sysfs.
fn identity(node: &DrmNode, name: &str) -> monitor::Identity {
    let card = node
        .dev_path()
        .and_then(|path| path.file_name().map(|name| name.to_os_string()));
    let card = card.unwrap_or_default();
    let path = Path::new("/sys/class/drm")
        .join(format!("{}-{name}", card.to_string_lossy()))
        .join("edid");
    let edid = fs::read(path).unwrap_or_default();
    monitor::Identity::from_edid(&edid)
}

/// Puts together the feedback of an output of the card `node`. Formats for the scanout tranche have
/// to work for us too, in case a buffer does not make it to a plane after all.
fn output_feedback(
    render_gpu: DrmNode,
    render_formats: &FormatSet,
    node: DrmNode,
    planes: &Planes,
) -> Result<OutputFeedback, std::io::Error> {
    let on_planes: FormatSet = planes
        .primary
        .iter()
        .chain(&planes.overlay)
        .flat_map(|plane| plane.formats.iter().copied())
        .filter(|format| render_formats.contains(format))
        .collect();
    let builder = DmabufFeedbackBuilder::new(render_gpu.dev_id(), render_formats.iter().copied());
    let render = builder.clone().build()?;
    let scanout = builder
        .add_preference_tranche(
            node.dev_id(),
            TrancheFlags::Scanout,
            on_planes.iter().copied(),
            4u32..=6,
        )
        .build()?;
    Ok(OutputFeedback { render, scanout })
}

/// The scale a rule asks for, as the kind of scale Smithay wants.
fn output_scale(scale: f64) -> Scale {
    if scale.fract() == 0.0 {
        Scale::Integer(scale as i32)
    } else {
        Scale::Fractional(scale)
    }
}

impl Drm {
    fn surface_mut(&mut self, node: DrmNode, crtc: crtc::Handle) -> Option<&mut Surface> {
        self.devices.get_mut(&node)?.surfaces.get_mut(&crtc)
    }

    fn device_added(
        &mut self,
        drm: &Rc<RefCell<Drm>>,
        state: &mut Wirbel,
        node: DrmNode,
        path: &Path,
    ) -> Result<(), Box<dyn Error>> {
        let flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
        let fd = self.session.open(path, flags)?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let (device, notifier) = DrmDevice::new(fd.clone(), true)?;
        let gbm = GbmDevice::new(fd)?;

        let events = drm.clone();
        let token = self
            .handle
            .insert_source(notifier, move |event, metadata, state| match event {
                DrmEvent::VBlank(crtc) => on_vblank(&events, state, node, crtc, metadata),
                DrmEvent::Error(err) => tracing::error!(%node, ?err, "DRM event"),
            })
            .map_err(|err| err.error)?;

        let render_node = self.try_render_node(node, &gbm);
        let allocator = match render_node {
            Some(_) => GbmAllocator::new(
                gbm.clone(),
                GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
            ),
            None => self
                .devices
                .values()
                .find(|device| device.render_node.is_some())
                .map(|device| device.manager.allocator().clone())
                .ok_or("no GPU can render so far")?,
        };
        let renders_here = render_node.is_some();
        let render_gpu = render_node
            .or(self.render_gpu)
            .ok_or("no GPU can render so far")?;
        tracing::info!(%node, ?path, ?render_node, driver = ?device.get_driver().ok().map(|d| d.name().to_string_lossy().into_owned()), "GPU added");

        let renderer = self.gpus.single_renderer(&render_gpu)?;
        let render_formats: FormatSet = renderer
            .as_ref()
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .filter(|format| renders_here || format.modifier == Modifier::Linear)
            .copied()
            .collect();
        drop(renderer);

        let exporter = GbmFramebufferExporter::new(gbm.clone(), render_node.into());
        let manager = DrmOutputManager::new(
            device,
            allocator,
            exporter,
            Some(gbm),
            FORMATS.iter().copied(),
            render_formats,
        );
        self.devices.insert(
            node,
            Device {
                manager,
                scanner: DrmScanner::new(),
                render_node,
                surfaces: HashMap::new(),
                token,
            },
        );
        self.device_changed(state, node);
        Ok(())
    }

    /// Sets up the GPU for rendering. Failing is no disaster, as long as another one can.
    fn try_render_node(&mut self, node: DrmNode, gbm: &GbmDevice<DrmDeviceFd>) -> Option<DrmNode> {
        let mut attempt = || -> Result<DrmNode, Box<dyn Error>> {
            // Safety: the display does not outlive the device it is made from.
            let display = unsafe { EGLDisplay::new(gbm.clone())? };
            let device = EGLDevice::device_for_display(&display)?;
            if device.is_software() {
                return Err("EGL runs on the CPU here".into());
            }
            let render_node = device.try_get_render_node()?.unwrap_or(node);
            self.gpus.as_mut().add_node(render_node, gbm.clone())?;
            Ok(render_node)
        };
        match attempt() {
            Ok(render_node) => {
                self.render_gpu.get_or_insert(render_node);
                Some(render_node)
            }
            Err(err) => {
                tracing::warn!(
                    %node, %err,
                    "this GPU cannot render. If it is NVIDIA: start wirbel in the Void stratum, \
                     the Arch stratum has no NVIDIA userspace"
                );
                None
            }
        }
    }

    /// Looks for connectors that came or went.
    fn device_changed(&mut self, state: &mut Wirbel, node: DrmNode) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };
        let events = match device.scanner.scan_connectors(device.manager.device()) {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!(%node, %err, "failed to scan the connectors");
                return;
            }
        };
        for event in events {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_connected(state, node, &connector, crtc),
                DrmScanEvent::Connected {
                    connector,
                    crtc: None,
                } => {
                    let name = connector_name(&connector);
                    tracing::error!(name, "no CRTC left for this connector");
                }
                DrmScanEvent::Disconnected {
                    crtc: Some(crtc), ..
                } => self.connector_disconnected(state, node, crtc),
                // New modes, for example when the EDID arrives late: start over with them.
                DrmScanEvent::Changed {
                    connector,
                    crtc: Some(crtc),
                } => {
                    self.connector_disconnected(state, node, crtc);
                    self.connector_connected(state, node, &connector, crtc);
                }
                _ => {}
            }
        }
    }

    fn connector_connected(
        &mut self,
        state: &mut Wirbel,
        node: DrmNode,
        connector: &connector::Info,
        crtc: crtc::Handle,
    ) {
        let name = connector_name(connector);
        let Some(render_gpu) = self.render_gpu else {
            return;
        };
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };

        let rule = state.config.monitor_rule(&name).cloned();
        let modes: Vec<ModeInfo> = connector.modes().iter().map(mode_info).collect();
        let Some(choice) = monitor::pick_mode(&modes, rule.as_ref()) else {
            tracing::warn!(name, "connected, but without any mode");
            return;
        };
        if let Some(warning) = &choice.warning {
            tracing::warn!(name, warning, "monitorrule not applied");
        }
        let drm_mode = connector.modes()[choice.index];
        let mode = Mode::from(drm_mode);

        let drm_device = device.manager.device();
        let mut planes = match drm_device.planes(&crtc) {
            Ok(planes) => planes,
            Err(err) => {
                tracing::error!(name, %err, "failed to query the planes");
                return;
            }
        };
        let nvidia = drm_device
            .get_driver()
            .is_ok_and(|driver| driver.name().to_string_lossy().contains("nvidia"));
        if nvidia {
            // Overlay planes on NVIDIA break the picture, says Smithay's own compositor.
            planes.overlay.clear();
        }

        let render_formats = self
            .gpus
            .single_renderer(&render_gpu)
            .map(|renderer| renderer.dmabuf_formats());
        let feedback = match render_formats {
            Ok(formats) => output_feedback(render_gpu, &formats, node, &planes)
                .inspect_err(|err| tracing::warn!(name, %err, "no dmabuf feedback"))
                .ok(),
            Err(err) => {
                tracing::warn!(name, ?err, "no dmabuf feedback");
                None
            }
        };

        let identity = identity(&node, &name);
        let (width, height) = connector.size().unwrap_or((0, 0));
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (width as i32, height as i32).into(),
                subpixel: connector.subpixel().into(),
                make: identity.make,
                model: identity.model,
                serial_number: identity.serial,
            },
        );
        let occupied = state
            .space
            .outputs()
            .filter_map(|output| state.space.output_geometry(output))
            .map(|geometry| (geometry.loc.x, geometry.size.w));
        let position = monitor::position_of(rule.as_ref(), occupied);
        let scale = rule.as_ref().and_then(|rule| rule.scale).map(output_scale);
        output.set_preferred(mode);
        output.change_current_state(Some(mode), None, scale, Some(position.into()));

        // The first frame is empty, the GPU that scans out draws it by itself.
        let target = device.render_node.unwrap_or(render_gpu);
        let mut renderer = match self.gpus.single_renderer(&target) {
            Ok(renderer) => renderer,
            Err(err) => {
                tracing::error!(name, ?err, "no renderer for this output");
                return;
            }
        };
        let elements = DrmOutputRenderElements::default();
        let mut initialize = |mode: DrmMode, planes| {
            device
                .manager
                .lock()
                .initialize_output::<_, SceneElement<Renderer<'_>>>(
                    crtc,
                    mode,
                    &[connector.handle()],
                    &output,
                    Some(planes),
                    &mut renderer,
                    &elements,
                )
        };
        let result = match initialize(drm_mode, planes.clone()) {
            Ok(drm_output) => Ok((drm_output, drm_mode)),
            // A rule can ask for what the link cannot carry. The default has the better chances.
            Err(err) => {
                let fallback = monitor::pick_mode(&modes, None)
                    .map(|choice| connector.modes()[choice.index])
                    .filter(|fallback| *fallback != drm_mode);
                tracing::warn!(name, ?err, ?fallback, "mode refused, trying the default");
                match fallback {
                    Some(fallback) => initialize(fallback, planes).map(|out| (out, fallback)),
                    None => Err(err),
                }
            }
        };
        let (drm_output, drm_mode) = match result {
            Ok(result) => result,
            Err(err) => {
                tracing::error!(name, ?err, "failed to set up the output");
                return;
            }
        };
        drop(renderer);
        let mode = Mode::from(drm_mode);
        output.set_preferred(mode);
        output.change_current_state(Some(mode), None, None, None);

        tracing::info!(
            name,
            width = mode.size.w,
            height = mode.size.h,
            refresh_mhz = mode.refresh,
            ?position,
            ?scale,
            "output added"
        );
        let global = output.create_global::<Wirbel>(&state.display_handle);
        device.surfaces.insert(
            crtc,
            Surface {
                output: output.clone(),
                global: Some(global),
                drm_output,
                pacing: Pacing::default(),
                presented: false,
                feedback,
                states: Rc::default(),
                scanout: false,
            },
        );
        state.add_output(&output);
        state.needs_redraw = true;
    }

    fn connector_disconnected(&mut self, state: &mut Wirbel, node: DrmNode, crtc: crtc::Handle) {
        let device = self.devices.get_mut(&node);
        let Some(mut surface) = device.and_then(|device| device.surfaces.remove(&crtc)) else {
            return;
        };
        tracing::info!(name = surface.output.name(), "output removed");
        state.remove_output(&surface.output);
        if let Some(global) = surface.global.take() {
            state.display_handle.remove_global::<Wirbel>(global);
        }
    }

    fn device_removed(&mut self, state: &mut Wirbel, node: DrmNode) {
        let Some(device) = self.devices.get(&node) else {
            return;
        };
        let crtcs: Vec<crtc::Handle> = device.surfaces.keys().copied().collect();
        for crtc in crtcs {
            self.connector_disconnected(state, node, crtc);
        }
        let Some(device) = self.devices.remove(&node) else {
            return;
        };
        self.handle.remove(device.token);
        if let Some(render_node) = device.render_node {
            self.gpus.as_mut().remove_node(&render_node);
            if self.render_gpu == Some(render_node) {
                tracing::error!(%node, "the rendering GPU went away");
                state.loop_signal.stop();
            }
        }
        tracing::info!(%node, "GPU removed");
    }

    fn pause(&mut self) {
        for device in self.devices.values_mut() {
            device.manager.pause();
            for surface in device.surfaces.values_mut() {
                surface.pacing.reset();
            }
        }
    }

    /// `blank`: a locked session must not show what the screen held before, not even until the
    /// first frame is up.
    fn resume(&mut self, state: &mut Wirbel, blank: bool) {
        let nodes: Vec<DrmNode> = self.devices.keys().copied().collect();
        for node in nodes {
            if let Some(device) = self.devices.get_mut(&node)
                && let Err(err) = device.manager.lock().activate(blank)
            {
                tracing::error!(%node, %err, "failed to take the GPU back");
                state.loop_signal.stop();
            }
            // Whatever happened to the connectors in the meantime.
            self.device_changed(state, node);
            // The damage tracker must not assume that the screen still shows the last frame.
            if let Some(device) = self.devices.get_mut(&node) {
                for surface in device.surfaces.values_mut() {
                    surface.drm_output.reset_buffers();
                    surface.pacing.reset();
                }
            }
        }
    }
}

/// The repaint goes after the clients had their turn. A copy to another GPU takes unknown time, so
/// there it starts at once.
fn repaint_delay(output: &Output, copies: bool) -> Duration {
    let refresh = output.current_mode().map_or(60_000, |mode| mode.refresh);
    if copies {
        Duration::ZERO
    } else {
        frame_duration(refresh).mul_f64(CLIENT_SHARE)
    }
}

fn frame_duration(refresh_mhz: i32) -> Duration {
    Duration::from_secs_f64(1_000.0 / f64::from(refresh_mhz.max(1_000)))
}

fn schedule_render(drm: &Rc<RefCell<Drm>>, node: DrmNode, crtc: crtc::Handle, delay: Duration) {
    let events = drm.clone();
    let render_later = move |_: _, _: &mut (), state: &mut Wirbel| {
        render(&events, state, node, crtc);
        TimeoutAction::Drop
    };
    let handle = drm.borrow().handle.clone();
    if let Err(err) = handle.insert_source(Timer::from_duration(delay), render_later) {
        tracing::error!(%err, "failed to schedule a frame");
    }
}

/// Starts a frame on every output that is free to.
fn request_redraw(drm: &Rc<RefCell<Drm>>) {
    let mut wanted = Vec::new();
    for (node, device) in &mut drm.borrow_mut().devices {
        for (crtc, surface) in &mut device.surfaces {
            if surface.pacing.request() {
                wanted.push((*node, *crtc));
            }
        }
    }
    for (node, crtc) in wanted {
        schedule_render(drm, node, crtc, Duration::ZERO);
    }
}

/// A frame left the screen's queue, or would have. Clients may go on.
fn frame_done(
    drm: &Rc<RefCell<Drm>>,
    state: &mut Wirbel,
    node: DrmNode,
    crtc: crtc::Handle,
    output: &Output,
    presented: Presented,
) {
    if presented.locked {
        state.lock_frame_presented(output);
    }
    state.send_frames(output);
    let dmabuf = drm
        .borrow_mut()
        .surface_mut(node, crtc)
        .and_then(|surface| Some((surface.feedback.clone()?, surface.states.clone())));
    if let Some((feedback, states)) = dmabuf {
        state.send_dmabuf_feedback(output, &states, &feedback.render, &feedback.scanout);
    }
    if presented.render {
        let copies = {
            let this = drm.borrow();
            let device = this.devices.get(&node);
            device.is_none_or(|device| device.render_node != this.render_gpu)
        };
        schedule_render(drm, node, crtc, repaint_delay(output, copies));
    }
}

/// Clients learn when their frame reached the screen, down to the kernel's clock if it says so.
fn present(
    feedback: &mut OutputPresentationFeedback,
    now: Time<Monotonic>,
    refresh_mhz: i32,
    metadata: Option<&DrmEventMetadata>,
) {
    let vblank = metadata.and_then(|metadata| match metadata.time {
        DrmEventTime::Monotonic(time) if !time.is_zero() => Some(Time::from(time)),
        _ => None,
    });
    let vsync = wp_presentation_feedback::Kind::Vsync;
    let (time, flags) = match vblank {
        Some(time) => (
            time,
            vsync
                | wp_presentation_feedback::Kind::HwClock
                | wp_presentation_feedback::Kind::HwCompletion,
        ),
        None => (now, vsync),
    };
    let sequence = metadata.map_or(0, |metadata| u64::from(metadata.sequence));
    let refresh = Refresh::fixed(frame_duration(refresh_mhz));
    feedback.presented(time, refresh, sequence, flags);
}

fn on_vblank(
    drm: &Rc<RefCell<Drm>>,
    state: &mut Wirbel,
    node: DrmNode,
    crtc: crtc::Handle,
    metadata: &mut Option<DrmEventMetadata>,
) {
    let (output, presented) = {
        let mut this = drm.borrow_mut();
        let Some(surface) = this.surface_mut(node, crtc) else {
            return;
        };
        match surface.drm_output.frame_submitted() {
            Ok(Some(Some(mut feedback))) => {
                let refresh = surface
                    .output
                    .current_mode()
                    .map_or(60_000, |mode| mode.refresh);
                present(&mut feedback, state.clock.now(), refresh, metadata.as_ref());
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(?err, "frame not submitted"),
        }
        if !surface.presented {
            surface.presented = true;
            tracing::info!(name = surface.output.name(), "first frame is on the screen");
        }
        (surface.output.clone(), surface.pacing.vblank())
    };
    frame_done(drm, state, node, crtc, &output, presented);
}

/// Waits out the vblank of a frame that did not need to be drawn.
fn estimate_vblank(drm: &Rc<RefCell<Drm>>, node: DrmNode, crtc: crtc::Handle, output: &Output) {
    let refresh = output.current_mode().map_or(60_000, |mode| mode.refresh);
    let (events, output) = (drm.clone(), output.clone());
    let timer = Timer::from_duration(frame_duration(refresh));
    let handle = drm.borrow().handle.clone();
    let inserted = handle.insert_source(timer, move |_, _, state| {
        let presented = events
            .borrow_mut()
            .surface_mut(node, crtc)
            .and_then(|surface| surface.pacing.estimated());
        if let Some(presented) = presented {
            frame_done(&events, state, node, crtc, &output, presented);
        }
        TimeoutAction::Drop
    });
    if let Err(err) = inserted {
        tracing::error!(%err, "failed to schedule a frame");
    }
}

enum Rendered {
    /// A frame is on its way to the screen.
    Queued,
    /// The screen already shows all of it.
    Unchanged,
    /// Nothing happened, and the device will be back.
    Retry,
    /// The GPU state does not fit, start over.
    Reset,
    Fatal,
}

fn classify(err: SwapBuffersError) -> Rendered {
    let denied = |err: &DrmError| {
        matches!(err, DrmError::Access(DrmAccessError { source, .. })
            if source.kind() == io::ErrorKind::PermissionDenied)
    };
    match err {
        SwapBuffersError::AlreadySwapped => Rendered::Retry,
        SwapBuffersError::TemporaryFailure(err) => match err.downcast_ref::<DrmError>() {
            Some(err) if matches!(err, DrmError::DeviceInactive) || denied(err) => Rendered::Retry,
            _ => {
                tracing::warn!(%err, "temporary failure while rendering");
                Rendered::Retry
            }
        },
        SwapBuffersError::ContextLost(err) => match err.downcast_ref::<DrmError>() {
            Some(DrmError::TestFailed(_)) => Rendered::Reset,
            _ => {
                tracing::error!(%err, "lost the renderer");
                Rendered::Fatal
            }
        },
    }
}

fn render(drm: &Rc<RefCell<Drm>>, state: &mut Wirbel, node: DrmNode, crtc: crtc::Handle) {
    let (output, outcome) = {
        let mut guard = drm.borrow_mut();
        let this = &mut *guard;
        if !this.session.is_active() {
            return;
        }
        let Some(render_gpu) = this.render_gpu else {
            return;
        };
        let Some(device) = this.devices.get_mut(&node) else {
            return;
        };
        let target = device.render_node.unwrap_or(render_gpu);
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };
        if !surface.pacing.start() {
            return;
        }
        let locked = state.lock.is_active();
        let output = surface.output.clone();
        let renderer = if target == render_gpu {
            this.gpus.single_renderer(&render_gpu)
        } else {
            let format = surface.drm_output.format();
            this.gpus.renderer(&render_gpu, &target, format)
        };
        let outcome = match renderer {
            Err(err) => {
                tracing::error!(?err, "no renderer");
                Rendered::Fatal
            }
            Ok(mut renderer) => {
                let elements = state.scene_elements(&mut renderer, &output);
                let color = state.config.background_color;
                let frame = surface.drm_output.render_frame(
                    &mut renderer,
                    &elements,
                    color,
                    FrameFlags::DEFAULT,
                );
                match frame {
                    Ok(frame) => {
                        let scanout =
                            matches!(frame.primary_element, PrimaryPlaneElement::Element(_));
                        if scanout != surface.scanout {
                            surface.scanout = scanout;
                            tracing::info!(name = output.name(), scanout, "direct scanout");
                        }
                        state.update_scanout_outputs(&output, &frame.states);
                        let mut feedback = state.presentation_feedback(&output, &frame.states);
                        surface.states = Rc::new(frame.states.clone());
                        if frame.is_empty {
                            // Nothing new reached the screen, nor will it.
                            feedback.discarded();
                            Rendered::Unchanged
                        } else {
                            match surface.drm_output.queue_frame(Some(feedback)) {
                                Ok(()) => Rendered::Queued,
                                Err(err) => classify(err.into()),
                            }
                        }
                    }
                    Err(RenderFrameError::PrepareFrame(err)) => classify(err.into()),
                    Err(RenderFrameError::RenderFrame(DamageError::Rendering(err))) => {
                        classify(err.into())
                    }
                    Err(err) => {
                        tracing::error!(?err, "failed to render");
                        Rendered::Retry
                    }
                }
            }
        };
        match outcome {
            Rendered::Queued => surface.pacing.queued(locked),
            Rendered::Unchanged => surface.pacing.unchanged(locked),
            Rendered::Retry => surface.pacing.unchanged(false),
            Rendered::Reset | Rendered::Fatal => surface.pacing.reset(),
        }
        if !matches!(outcome, Rendered::Reset | Rendered::Fatal)
            && let Ok(mut renderer) = this.gpus.single_renderer(&render_gpu)
        {
            this.capturer.run(state, &mut renderer, &output);
        }
        (output, outcome)
    };

    match outcome {
        Rendered::Queued => {}
        Rendered::Unchanged => estimate_vblank(drm, node, crtc, &output),
        Rendered::Retry => {
            // This frame is lost, so it has to happen again.
            state.needs_redraw = true;
            estimate_vblank(drm, node, crtc, &output);
        }
        Rendered::Reset => {
            // After a VT switch the other master may have left the CRTCs in a different state.
            let mut this = drm.borrow_mut();
            let reset = this
                .devices
                .get_mut(&node)
                .map(|device| device.manager.device_mut().reset_state());
            drop(this);
            match reset {
                Some(Ok(())) => {
                    state.needs_redraw = true;
                }
                other => {
                    tracing::error!(?other, "failed to reset the GPU");
                    state.loop_signal.stop();
                }
            }
        }
        Rendered::Fatal => state.loop_signal.stop(),
    }
}

/// Every device on the seat, the one that should render first.
fn ordered_devices(udev: &UdevBackend, preferred: Option<&Path>) -> Vec<(DrmNode, PathBuf)> {
    let mut devices: Vec<(DrmNode, PathBuf)> = udev
        .device_list()
        .filter_map(|(id, path)| {
            let node = DrmNode::from_dev_id(id)
                .inspect_err(|err| tracing::warn!(%err, ?path, "skipping device"));
            Some((node.ok()?, path.to_path_buf()))
        })
        .collect();
    devices.sort_by_key(|(_, path)| (Some(path.as_path()) != preferred, path.clone()));
    devices
}

fn on_session_event(
    drm: &Rc<RefCell<Drm>>,
    libinput: &mut Libinput,
    active: &SessionActive,
    state: &mut Wirbel,
    event: SessionEvent,
) {
    match event {
        SessionEvent::PauseSession => {
            tracing::info!("session paused");
            active.store(false, Ordering::Relaxed);
            libinput.suspend();
            drm.borrow_mut().pause();
        }
        SessionEvent::ActivateSession => {
            tracing::info!(locked = state.lock.is_active(), "session resumed");
            active.store(true, Ordering::Relaxed);
            if libinput.resume().is_err() {
                tracing::error!("failed to resume the input devices");
            }
            drm.borrow_mut().resume(state, state.lock.is_active());
            state.needs_redraw = true;
        }
    }
}

fn on_udev_event(drm: &Rc<RefCell<Drm>>, state: &mut Wirbel, event: UdevEvent) {
    let mut this = drm.borrow_mut();
    match event {
        UdevEvent::Added { device_id, path } => match DrmNode::from_dev_id(device_id) {
            Ok(node) => {
                if let Err(err) = this.device_added(drm, state, node, &path) {
                    tracing::error!(%err, ?path, "skipping device");
                }
            }
            Err(err) => tracing::error!(%err, ?path, "skipping device"),
        },
        UdevEvent::Changed { device_id } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                tracing::info!(%node, "hotplug");
                this.device_changed(state, node);
            }
        }
        UdevEvent::Removed { device_id } => {
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                this.device_removed(state, node);
            }
        }
    }
    state.needs_redraw = true;
}

fn insert_input_sources(
    event_loop: &EventLoop<'static, Wirbel>,
    drm: &Rc<RefCell<Drm>>,
    notifier: LibSeatSessionNotifier,
    active: SessionActive,
) -> Result<(), Box<dyn Error>> {
    let (session, seat) = {
        let this = drm.borrow();
        (this.session.clone(), this.session.seat())
    };
    let mut libinput =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.into());
    libinput
        .udev_assign_seat(&seat)
        .map_err(|()| "libinput cannot use this seat")?;
    let backend = LibinputInputBackend::new(libinput.clone());

    let handle = event_loop.handle();
    let events = drm.clone();
    handle
        .insert_source(backend, move |mut event, _, state| {
            match &mut event {
                InputEvent::DeviceAdded { device } => {
                    if device.has_capability(DeviceCapability::Keyboard) {
                        if let Some(keyboard) = state.seat.get_keyboard() {
                            device.led_update(keyboard.led_state().into());
                        }
                        events.borrow_mut().keyboards.push(device.clone());
                    }
                }
                InputEvent::DeviceRemoved { device } => {
                    events
                        .borrow_mut()
                        .keyboards
                        .retain(|other| other != device);
                }
                _ => {}
            }
            state.process_input_event(event);
        })
        .map_err(|err| err.error)?;

    let events = drm.clone();
    handle
        .insert_source(notifier, move |event, _, state| {
            on_session_event(&events, &mut libinput, &active, state, event);
        })
        .map_err(|err| err.error)?;
    Ok(())
}

/// Takes over the screen. Errors arrive before anything is lost: the caller reports them and
/// leaves, elogind gives the VT back.
pub fn init(
    event_loop: &mut EventLoop<'static, Wirbel>,
    state: &mut Wirbel,
    active: SessionActive,
) -> Result<(), Box<dyn Error>> {
    let (session, notifier) = LibSeatSession::new()
        .map_err(|err| format!("no session from libseat: {err}. See docs/TTY-TEST.md"))?;
    let seat = session.seat();
    tracing::info!(seat, "got a session");

    let udev = UdevBackend::new(&seat)?;
    let preferred = preferred_gpu(&seat, state.config.render_gpu.as_deref());
    tracing::info!(?preferred, "render GPU to try first");

    let gpus = GpuManager::new(GbmGlesBackend::with_context_priority(ContextPriority::High))
        .map_err(|err| format!("no GPU manager: {err:?}"))?;
    let drm = Rc::new(RefCell::new(Drm {
        session,
        gpus,
        render_gpu: None,
        devices: HashMap::new(),
        capturer: Capturer::default(),
        keyboards: Vec::new(),
        handle: event_loop.handle(),
    }));

    for (node, path) in ordered_devices(&udev, preferred.as_deref()) {
        if let Err(err) = drm.borrow_mut().device_added(&drm, state, node, &path) {
            tracing::error!(%err, ?path, "skipping device");
        }
    }

    {
        let mut guard = drm.borrow_mut();
        let this = &mut *guard;
        let render_gpu = this.render_gpu.ok_or("no GPU can render")?;
        tracing::info!(%render_gpu, "rendering GPU");
        let renderer = this
            .gpus
            .single_renderer(&render_gpu)
            .map_err(|err| format!("no renderer: {err:?}"))?;
        state.shm_state.update_formats(renderer.shm_formats());
        let formats = renderer.dmabuf_formats();
        drop(renderer);
        state.offer_dmabufs(render_gpu.dev_id(), formats);
        let rendering = this
            .devices
            .values()
            .find(|device| device.render_node == Some(render_gpu));
        if let Some(device) = rendering {
            state.offer_syncobj(device.manager.device().device_fd().clone());
        }
        if !this
            .devices
            .values()
            .any(|device| !device.surfaces.is_empty())
        {
            return Err("no output came up: check the connectors in `wirbel check`".into());
        }
    }

    state.cursors = Some(Cursors::load(
        state.config.cursor_theme.as_deref(),
        state.config.cursor_size,
    ));
    state.redraw = {
        let drm = drm.clone();
        Box::new(move || request_redraw(&drm))
    };
    state.import_dmabuf = {
        let drm = drm.clone();
        Box::new(move |dmabuf| {
            let mut guard = drm.borrow_mut();
            let this = &mut *guard;
            let Some(render_gpu) = this.render_gpu else {
                return false;
            };
            let imported = this
                .gpus
                .single_renderer(&render_gpu)
                .map_err(|err| format!("{err:?}"))
                .and_then(|mut renderer| {
                    let result = renderer.import_dmabuf(dmabuf, None);
                    result.map(|_| ()).map_err(|err| format!("{err:?}"))
                });
            match imported {
                Ok(()) => {
                    if dmabuf.node().is_none() {
                        dmabuf.set_node(render_gpu);
                    }
                    true
                }
                Err(err) => {
                    tracing::warn!(err, "failed to import a dmabuf");
                    false
                }
            }
        })
    };
    state.early_import = {
        let drm = drm.clone();
        Box::new(move |surface| {
            let mut guard = drm.borrow_mut();
            let this = &mut *guard;
            if let Some(render_gpu) = this.render_gpu
                && let Err(err) = this.gpus.early_import(render_gpu, surface)
            {
                tracing::debug!(?err, "early import failed");
            }
        })
    };
    state.change_vt = {
        let mut session = drm.borrow().session.clone();
        Some(Box::new(move |vt| {
            tracing::info!(vt, "switching VT");
            if let Err(err) = session.change_vt(vt) {
                tracing::error!(%err, vt, "failed to switch the VT");
            }
        }))
    };
    state.led_update = {
        let drm = drm.clone();
        Box::new(move |leds| {
            for keyboard in &mut drm.borrow_mut().keyboards {
                keyboard.led_update(leds.into());
            }
        })
    };

    insert_input_sources(event_loop, &drm, notifier, active)?;
    let events = drm.clone();
    event_loop
        .handle()
        .insert_source(udev, move |event, _, state| {
            on_udev_event(&events, state, event);
        })
        .map_err(|err| err.error)?;
    Ok(())
}
