//! Render override: something other than the scene draws an output, with raw GLES calls in the
//! renderer's own context. The core does not know what it draws. While an override is set for an
//! output and the session is not locked, `Wirbel::output_elements` gives the backend the cursor and
//! one element covering the whole output, whose `draw` runs the override.
//!
//! An override must not take the compositor down: a panic or an error in it is logged, the
//! override is marked broken and is not drawn again, and its owner hears about it through
//! `SceneOverride::failed`. The GL state the renderer relies on is saved before and put back after
//! every draw, even when the override panicked.
//!
//! An override taken off its output with `Wirbel::set_scene_override` gets one more call in its
//! context, `SceneOverride::release`, on the next frame of that output, so that it can delete
//! what it made: an element in front of everything that draws nothing.
//!
//! An override may keep layer shell surfaces (bars, notifications) on the screen
//! (`SceneOverride::layers_above`): the core draws them over it, flat at their place as on the 2D
//! desktop, and while an input filter routes the input they get the pointer first, since they
//! are on top (`Wirbel::override_layers`).
//!
//! The same trait can draw an overlay instead (`SceneOverrideSlot::overlay`, set with
//! `Wirbel::set_overlay`): over whatever the output shows (the scene or an override), in front of
//! everything, covering nothing it does not draw over. It is guarded and released the same way.

use std::{
    cell::Cell,
    fmt,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

use smithay::{
    backend::{
        drm::DrmDeviceFd,
        renderer::{
            ImportAll, ImportMem, Renderer,
            element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
            gles::{GlesError, GlesFrame, GlesRenderer, ffi},
            multigpu::{self, MultiFrame, MultiRenderer, gbm::GbmGlesBackend},
            utils::{CommitCounter, DamageSet, OpaqueRegions},
        },
    },
    output::Output,
    utils::{Buffer as BufferCoords, Physical, Point, Rectangle, Scale, Size, Transform},
    wayland::shell::wlr_layer::Layer,
};

use crate::render::SceneElement;

/// Where an override draws.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverrideTarget {
    /// The size of the output as the user sees it, physical pixels.
    pub size: Size<i32, Physical>,
    /// How that picture is turned and mirrored on its way into the framebuffer, as Smithay
    /// draws it: a point at `(x, y)` in normalised device coordinates of the picture, `y` up,
    /// ends up at `flip_y(transform.matrix() * (x, y))` in the framebuffer.
    pub transform: Transform,
    /// The size of the bound framebuffer, physical pixels: `size` turned by `transform`.
    pub framebuffer: Size<i32, Physical>,
    /// The scale of the output.
    pub scale: f64,
}

/// What draws an output instead of the scene.
pub trait SceneOverride {
    /// Runs right before `draw` with Smithay's renderer, to draw into offscreen buffers with it
    /// (client surfaces, say). The frame's framebuffer, viewport and the GL state are put back
    /// afterwards. An error or a panic breaks the override, as in `draw`.
    fn prepare(
        &self,
        _renderer: &mut GlesRenderer,
        _target: &OverrideTarget,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Draws the whole output into the bound framebuffer. The GL context is current. The state
    /// of the renderer is saved and restored around this call; what the override creates (programs,
    /// buffers, framebuffers) it has to clean up itself.
    fn draw(&self, gl: &ffi::Gles2, target: &OverrideTarget) -> Result<(), String>;

    /// Called once when `draw` failed or panicked. The override is broken from then on: the scene
    /// is drawn instead until the owner sets a new one. The owner may want to change its state,
    /// which this method has no access to; scheduling that on the event loop works.
    fn failed(&self, error: &str);

    /// Called once with the context current after the override was taken off its output (also
    /// after it failed), to delete the GL objects it made. The GL state is saved and restored
    /// around it as around `draw`. It must not draw.
    fn release(&self, _gl: &ffi::Gles2) {}

    /// The layer shell layers whose surfaces stay on the screen over the override. None by
    /// default: the override covers everything.
    fn layers_above(&self) -> Vec<Layer> {
        Vec::new()
    }
}

/// An override for one output, as kept in `Wirbel::scene_override`.
pub struct SceneOverrideSlot {
    output: Output,
    scene: Rc<dyn SceneOverride>,
    id: Id,
    commit: Cell<CommitCounter>,
    broken: Rc<Cell<bool>>,
    /// Whether `release` ran, after the slot was retired.
    released: Rc<Cell<bool>>,
    /// Drawn over the output's picture instead of in place of the scene.
    over: bool,
}

impl fmt::Debug for SceneOverrideSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SceneOverrideSlot")
            .field("output", &self.output.name())
            .field("broken", &self.broken.get())
            .finish_non_exhaustive()
    }
}

impl SceneOverrideSlot {
    pub fn new(output: Output, scene: Rc<dyn SceneOverride>) -> Self {
        Self {
            output,
            scene,
            id: Id::new(),
            commit: Cell::new(CommitCounter::default()),
            broken: Rc::new(Cell::new(false)),
            released: Rc::new(Cell::new(false)),
            over: false,
        }
    }

    /// An overlay for `output`: `scene` draws over the output's picture, which stays (see the
    /// module documentation).
    pub fn overlay(output: Output, scene: Rc<dyn SceneOverride>) -> Self {
        Self {
            over: true,
            ..Self::new(output, scene)
        }
    }

    /// Whether this is an overlay.
    pub fn is_overlay(&self) -> bool {
        self.over
    }

    /// Whether this slot holds `scene`.
    pub fn holds(&self, scene: &Rc<dyn SceneOverride>) -> bool {
        Rc::ptr_eq(&self.scene, scene)
    }

    /// Whether `release` ran.
    pub fn is_released(&self) -> bool {
        self.released.get()
    }

    pub fn output(&self) -> &Output {
        &self.output
    }

    /// See `SceneOverride::layers_above`.
    pub fn layers_above(&self) -> Vec<Layer> {
        self.scene.layers_above()
    }

    /// Whether the override failed. A broken override is not drawn.
    pub fn is_broken(&self) -> bool {
        self.broken.get()
    }

    /// Breaks the override from outside a frame (its output went away, say): its owner hears
    /// about it through `SceneOverride::failed`, once, as after a failed `draw`.
    pub fn fail(&self, error: &str) {
        fail(&self.broken, &*self.scene, error);
    }

    /// The element that draws the override, for one frame. Every frame of an override is a new
    /// picture, so every element damages the whole output.
    pub fn element(&self) -> OverrideElement {
        self.make_element(false)
    }

    /// For a retired slot: the element that runs `release` instead of drawing. It damages the
    /// whole output, which leaving an override does anyway, and is opaque nowhere, so that
    /// nothing in front of it hides it from the damage tracker.
    pub fn release_element(&self) -> OverrideElement {
        self.make_element(true)
    }

    fn make_element(&self, release: bool) -> OverrideElement {
        let mut commit = self.commit.get();
        commit.increment();
        self.commit.set(commit);
        let size = self
            .output
            .current_mode()
            .map_or_else(Size::default, |mode| {
                self.output.current_transform().transform_size(mode.size)
            });
        OverrideElement {
            id: self.id.clone(),
            commit,
            size,
            scale: self.output.current_scale().fractional_scale(),
            scene: self.scene.clone(),
            broken: self.broken.clone(),
            release: release.then(|| self.released.clone()),
            over: self.over,
        }
    }
}

impl crate::Wirbel {
    /// Puts `slot` in place of the current override, or just removes that one. A removed
    /// override stays around until its `release` ran on a frame of its output.
    pub fn set_scene_override(&mut self, slot: Option<SceneOverrideSlot>) {
        self.retired_overrides.retain(|old| !old.is_released());
        if let Some(old) = self.scene_override.take() {
            self.retired_overrides.push(old);
        }
        if let Some(new) = &slot {
            // Back before it was released: it keeps what it made.
            self.retired_overrides
                .retain(|old| !Rc::ptr_eq(&old.scene, &new.scene));
        }
        self.scene_override = slot;
    }

    /// Puts `slot` (made with `SceneOverrideSlot::overlay`) in place of the current overlay, or
    /// just removes that one; a removed overlay is released like an override.
    pub fn set_overlay(&mut self, slot: Option<SceneOverrideSlot>) {
        self.retired_overrides.retain(|old| !old.is_released());
        if let Some(old) = self.overlay.take() {
            self.retired_overrides.push(old);
        }
        if let Some(new) = &slot {
            self.retired_overrides
                .retain(|old| !Rc::ptr_eq(&old.scene, &new.scene));
        }
        self.overlay = slot;
    }

    /// The elements that release the overrides retired from `output`, for the front of the list.
    pub fn release_elements<'a>(
        &'a self,
        output: &'a Output,
    ) -> impl Iterator<Item = OverrideElement> + 'a {
        self.retired_overrides
            .iter()
            .filter(move |slot| slot.output() == output && !slot.is_released())
            .map(SceneOverrideSlot::release_element)
    }
}

/// Draws a `SceneOverride`, see `SceneOverrideSlot::element`.
pub struct OverrideElement {
    id: Id,
    commit: CommitCounter,
    size: Size<i32, Physical>,
    scale: f64,
    scene: Rc<dyn SceneOverride>,
    broken: Rc<Cell<bool>>,
    /// Set for a release element: whether the release ran.
    release: Option<Rc<Cell<bool>>>,
    /// An overlay's: opaque nowhere.
    over: bool,
}

impl fmt::Debug for OverrideElement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OverrideElement")
            .field("id", &self.id)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// How many overrides failed, for the tests.
static FAILURES: AtomicU64 = AtomicU64::new(0);

/// Marks an override broken and tells its owner, once.
fn fail(broken: &Cell<bool>, scene: &dyn SceneOverride, error: &str) {
    if broken.replace(true) {
        return;
    }
    FAILURES.fetch_add(1, Ordering::Relaxed);
    tracing::error!(error, "render override failed, drawing the scene instead");
    if let Err(panic) = crate::safety::contain_panic(|| scene.failed(error)) {
        tracing::error!(panic, "render override panicked while failing");
    }
}

/// How many times an override broke since the start.
pub fn failures() -> u64 {
    FAILURES.load(Ordering::Relaxed)
}

impl OverrideElement {
    /// Whether this element only releases a retired override.
    pub fn is_release(&self) -> bool {
        self.release.is_some()
    }

    /// Runs `release` once, guarded like `draw`.
    fn release(&self, gl: &ffi::Gles2, released: &Cell<bool>) {
        if !released.replace(true) {
            let _saved = GlState::save(gl);
            if let Err(panic) = crate::safety::contain_panic(|| self.scene.release(gl)) {
                tracing::error!(panic, "render override panicked while releasing");
            }
        }
    }

    /// Runs one step of the override with everything that guards the compositor from it.
    fn guarded(&self, gl: &ffi::Gles2, step: impl FnOnce() -> Result<(), String>) {
        if self.broken.get() {
            return;
        }
        let result = {
            let _saved = GlState::save(gl);
            crate::safety::contain_panic(step)
        };
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.fail(&error),
            Err(panic) => self.fail(&format!("panic: {panic}")),
        }
    }

    /// Marks the override broken and tells its owner, once.
    fn fail(&self, error: &str) {
        fail(&self.broken, &*self.scene, error);
    }

    fn draw_gles(&self, frame: &mut GlesFrame<'_, '_>) -> Result<(), GlesError> {
        use smithay::backend::renderer::{Frame, FrameContext};
        let size = frame.output_size();
        let transform = frame.transformation();
        let target = OverrideTarget {
            size,
            transform,
            framebuffer: transform.transform_size(size),
            scale: self.scale,
        };
        // The function table, to save the GL state around `prepare` while it has the renderer.
        let gl = frame.with_context(|gl| gl.clone())?;
        if let Some(released) = &self.release {
            self.release(&gl, released);
            return Ok(());
        }
        {
            let mut renderer = frame.renderer();
            let renderer = renderer.as_mut();
            self.guarded(&gl, || self.scene.prepare(renderer, &target));
        }
        frame.with_context(|gl| self.guarded(gl, || self.scene.draw(gl, &target)))
    }
}

impl Element for OverrideElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, BufferCoords> {
        Rectangle::from_size(
            self.size
                .to_f64()
                .to_logical(1.0)
                .to_buffer(1.0, Transform::Normal),
        )
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        Rectangle::from_size(self.size)
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        _commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        DamageSet::from_slice(&[Rectangle::from_size(self.geometry(scale).size)])
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        if self.broken.get() || self.release.is_some() || self.over {
            return OpaqueRegions::default();
        }
        OpaqueRegions::from_slice(&[Rectangle::from_size(self.geometry(scale).size)])
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<GlesRenderer> for OverrideElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, BufferCoords>,
        _dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), GlesError> {
        self.draw_gles(frame)
    }
}

/// The renderer of the DRM backend.
type DrmApi = GbmGlesBackend<GlesRenderer, DrmDeviceFd>;

impl<'render, 'target> RenderElement<MultiRenderer<'render, 'target, DrmApi, DrmApi>>
    for OverrideElement
{
    fn draw(
        &self,
        frame: &mut MultiFrame<'render, 'target, '_, '_, DrmApi, DrmApi>,
        _src: Rectangle<f64, BufferCoords>,
        _dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), multigpu::Error<DrmApi, DrmApi>> {
        // The frame of the render GPU; MultiRenderer copies it to the target GPU afterwards.
        self.draw_gles(frame.as_mut())
            .map_err(multigpu::Error::Render)
    }
}

/// What a backend draws on an output: the scene, or the cursor and an override.
// Like the enums of Smithay's `render_elements!`: boxing would allocate for every element.
#[allow(clippy::large_enum_variant)]
pub enum OutputElement<R>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
{
    Scene(SceneElement<R>),
    Override(OverrideElement),
}

impl<R> fmt::Debug for OutputElement<R>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scene(_) => f.write_str("OutputElement::Scene"),
            Self::Override(element) => element.fmt(f),
        }
    }
}

impl<R> From<SceneElement<R>> for OutputElement<R>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
{
    fn from(element: SceneElement<R>) -> Self {
        Self::Scene(element)
    }
}

/// Delegates to the element inside.
macro_rules! both {
    ($self:ident, $element:ident => $call:expr) => {
        match $self {
            OutputElement::Scene($element) => $call,
            OutputElement::Override($element) => $call,
        }
    };
}

impl<R> Element for OutputElement<R>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
{
    fn id(&self) -> &Id {
        both!(self, e => e.id())
    }

    fn current_commit(&self) -> CommitCounter {
        both!(self, e => e.current_commit())
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        both!(self, e => e.location(scale))
    }

    fn src(&self) -> Rectangle<f64, BufferCoords> {
        both!(self, e => e.src())
    }

    fn transform(&self) -> Transform {
        both!(self, e => e.transform())
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        both!(self, e => e.geometry(scale))
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        both!(self, e => e.damage_since(scale, commit))
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        both!(self, e => e.opaque_regions(scale))
    }

    fn alpha(&self) -> f32 {
        both!(self, e => e.alpha())
    }

    fn kind(&self) -> Kind {
        both!(self, e => e.kind())
    }

    fn is_framebuffer_effect(&self) -> bool {
        both!(self, e => e.is_framebuffer_effect())
    }
}

impl<R> RenderElement<R> for OutputElement<R>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
    SceneElement<R>: RenderElement<R>,
    OverrideElement: RenderElement<R>,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), R::Error> {
        both!(self, e => e.draw(frame, src, dst, damage, opaque_regions, cache))
    }

    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        both!(self, e => e.underlying_storage(renderer))
    }

    fn capture_framebuffer(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        cache: &smithay::utils::user_data::UserDataMap,
    ) -> Result<(), R::Error> {
        both!(self, e => e.capture_framebuffer(frame, src, dst, cache))
    }
}

/// The GL state Smithay's renderer relies on between its own calls, saved when made and put back
/// when dropped, also while unwinding from a panic.
struct GlState<'a> {
    gl: &'a ffi::Gles2,
    ints: [ffi::types::GLint; INTS.len()],
    enabled: [bool; CAPS.len()],
    viewport: [ffi::types::GLint; 4],
    scissor: [ffi::types::GLint; 4],
    color_mask: [ffi::types::GLboolean; 4],
    depth_mask: ffi::types::GLboolean,
    clear_color: [f32; 4],
    depth_clear: f32,
    texture: ffi::types::GLint,
}

/// Plain integer state, in the order it is put back.
const INTS: [ffi::types::GLenum; 17] = [
    ffi::CURRENT_PROGRAM,
    ffi::VERTEX_ARRAY_BINDING,
    ffi::ARRAY_BUFFER_BINDING,
    ffi::DRAW_FRAMEBUFFER_BINDING,
    ffi::READ_FRAMEBUFFER_BINDING,
    ffi::RENDERBUFFER_BINDING,
    ffi::ACTIVE_TEXTURE,
    ffi::BLEND_SRC_RGB,
    ffi::BLEND_DST_RGB,
    ffi::BLEND_SRC_ALPHA,
    ffi::BLEND_DST_ALPHA,
    ffi::BLEND_EQUATION_RGB,
    ffi::BLEND_EQUATION_ALPHA,
    ffi::DEPTH_FUNC,
    ffi::CULL_FACE_MODE,
    ffi::FRONT_FACE,
    ffi::UNPACK_ALIGNMENT,
];

const CAPS: [ffi::types::GLenum; 6] = [
    ffi::BLEND,
    ffi::DEPTH_TEST,
    ffi::SCISSOR_TEST,
    ffi::CULL_FACE,
    ffi::STENCIL_TEST,
    ffi::DITHER,
];

impl<'a> GlState<'a> {
    fn save(gl: &'a ffi::Gles2) -> Self {
        let mut state = Self {
            gl,
            ints: [0; INTS.len()],
            enabled: [false; CAPS.len()],
            viewport: [0; 4],
            scissor: [0; 4],
            color_mask: [ffi::TRUE; 4],
            depth_mask: ffi::TRUE,
            clear_color: [0.0; 4],
            depth_clear: 1.0,
            texture: 0,
        };
        // Safety: plain queries into buffers of the right size, in the current context.
        unsafe {
            // Errors left by others must not be blamed on the override.
            while gl.GetError() != ffi::NO_ERROR {}
            for (name, value) in INTS.iter().zip(state.ints.iter_mut()) {
                gl.GetIntegerv(*name, value);
            }
            for (cap, enabled) in CAPS.iter().zip(state.enabled.iter_mut()) {
                *enabled = gl.IsEnabled(*cap) == ffi::TRUE;
            }
            gl.GetIntegerv(ffi::VIEWPORT, state.viewport.as_mut_ptr());
            gl.GetIntegerv(ffi::SCISSOR_BOX, state.scissor.as_mut_ptr());
            gl.GetBooleanv(ffi::COLOR_WRITEMASK, state.color_mask.as_mut_ptr());
            gl.GetBooleanv(ffi::DEPTH_WRITEMASK, &mut state.depth_mask);
            gl.GetFloatv(ffi::COLOR_CLEAR_VALUE, state.clear_color.as_mut_ptr());
            gl.GetFloatv(ffi::DEPTH_CLEAR_VALUE, &mut state.depth_clear);
            gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut state.texture);
        }
        state
    }
}

impl Drop for GlState<'_> {
    fn drop(&mut self) {
        let gl = self.gl;
        let int = |index: usize| self.ints[index];
        let uint = |index: usize| self.ints[index] as ffi::types::GLuint;
        // Safety: restores values read from this context in `save`.
        unsafe {
            gl.UseProgram(uint(0));
            gl.BindVertexArray(uint(1));
            gl.BindBuffer(ffi::ARRAY_BUFFER, uint(2));
            gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, uint(3));
            gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, uint(4));
            gl.BindRenderbuffer(ffi::RENDERBUFFER, uint(5));
            gl.ActiveTexture(uint(6));
            gl.BindTexture(ffi::TEXTURE_2D, self.texture as ffi::types::GLuint);
            gl.BlendFuncSeparate(uint(7), uint(8), uint(9), uint(10));
            gl.BlendEquationSeparate(uint(11), uint(12));
            gl.DepthFunc(uint(13));
            gl.CullFace(uint(14));
            gl.FrontFace(uint(15));
            gl.PixelStorei(ffi::UNPACK_ALIGNMENT, int(16));
            for (cap, enabled) in CAPS.iter().zip(self.enabled) {
                if enabled {
                    gl.Enable(*cap);
                } else {
                    gl.Disable(*cap);
                }
            }
            let [x, y, w, h] = self.viewport;
            gl.Viewport(x, y, w, h);
            let [x, y, w, h] = self.scissor;
            gl.Scissor(x, y, w, h);
            let [r, g, b, a] = self.color_mask;
            gl.ColorMask(r, g, b, a);
            gl.DepthMask(self.depth_mask);
            let [r, g, b, a] = self.clear_color;
            gl.ClearColor(r, g, b, a);
            gl.ClearDepthf(self.depth_clear);
        }
    }
}

#[cfg(test)]
mod tests {
    use smithay::output::{Mode, PhysicalProperties, Subpixel};

    use super::*;

    struct Recorder {
        failed: Cell<u32>,
    }

    impl SceneOverride for Recorder {
        fn draw(&self, _gl: &ffi::Gles2, _target: &OverrideTarget) -> Result<(), String> {
            Ok(())
        }

        fn failed(&self, _error: &str) {
            self.failed.set(self.failed.get() + 1);
        }
    }

    fn output(transform: Transform) -> Output {
        let output = Output::new(
            "test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "test".into(),
                serial_number: "0".into(),
            },
        );
        let mode = Mode {
            size: (800, 600).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), Some(transform), None, None);
        output
    }

    #[test]
    fn every_element_is_a_new_picture_of_the_whole_output() {
        let recorder = Rc::new(Recorder {
            failed: Cell::new(0),
        });
        let slot = SceneOverrideSlot::new(output(Transform::_90), recorder);
        let first = slot.element();
        let second = slot.element();
        assert_eq!(first.id(), second.id());
        assert_ne!(first.current_commit(), second.current_commit());
        let scale = Scale::from(1.0);
        // Turned by 90°, the user sees the output upright.
        assert_eq!(
            second.geometry(scale),
            Rectangle::from_size((600, 800).into())
        );
        assert_eq!(second.opaque_regions(scale).len(), 1);
        assert_eq!(
            second
                .damage_since(scale, Some(second.current_commit()))
                .len(),
            1
        );
    }

    #[test]
    fn an_overlay_covers_nothing() {
        let recorder = Rc::new(Recorder {
            failed: Cell::new(0),
        });
        let slot = SceneOverrideSlot::overlay(output(Transform::Normal), recorder);
        assert!(slot.is_overlay());
        let element = slot.element();
        let scale = Scale::from(1.0);
        assert_eq!(
            element.geometry(scale),
            Rectangle::from_size((800, 600).into())
        );
        assert!(element.opaque_regions(scale).is_empty());
        assert!(
            !SceneOverrideSlot::new(output(Transform::Normal), slot.scene.clone()).is_overlay()
        );
    }

    #[test]
    fn a_failure_breaks_the_override_once() {
        let recorder = Rc::new(Recorder {
            failed: Cell::new(0),
        });
        let slot = SceneOverrideSlot::new(output(Transform::Normal), recorder.clone());
        let element = slot.element();
        let before = failures();
        element.fail("first");
        element.fail("second");
        assert!(slot.is_broken());
        assert_eq!(recorder.failed.get(), 1);
        // Other tests may fail overrides at the same time.
        assert!(failures() > before);
        // A broken override hides nothing.
        assert!(slot.element().opaque_regions(Scale::from(1.0)).is_empty());
    }

    #[test]
    fn a_release_element_damages_all_and_hides_nothing() {
        let recorder: Rc<dyn SceneOverride> = Rc::new(Recorder {
            failed: Cell::new(0),
        });
        let slot = SceneOverrideSlot::new(output(Transform::Normal), recorder.clone());
        assert!(slot.holds(&recorder));
        let release = slot.release_element();
        assert!(release.is_release() && !slot.element().is_release());
        let scale = Scale::from(1.0);
        assert!(release.opaque_regions(scale).is_empty());
        assert_eq!(
            release.damage_since(scale, None).iter().collect::<Vec<_>>(),
            [&Rectangle::from_size((800, 600).into())]
        );
        assert!(!slot.is_released());
    }

    struct Panicking;

    impl SceneOverride for Panicking {
        fn draw(&self, _gl: &ffi::Gles2, _target: &OverrideTarget) -> Result<(), String> {
            Ok(())
        }

        fn failed(&self, _error: &str) {
            panic!("failing badly");
        }
    }

    #[test]
    fn a_slot_can_be_failed_from_outside_once() {
        let recorder = Rc::new(Recorder {
            failed: Cell::new(0),
        });
        let slot = SceneOverrideSlot::new(output(Transform::Normal), recorder.clone());
        slot.fail("output removed");
        slot.fail("again");
        assert!(slot.is_broken());
        assert_eq!(recorder.failed.get(), 1);
    }

    #[test]
    fn a_panic_while_failing_is_contained() {
        let slot = SceneOverrideSlot::new(output(Transform::Normal), Rc::new(Panicking));
        slot.element().fail("x");
        assert!(slot.is_broken());
    }
}
