//! Backends connect the compositor to a display and input devices.

pub mod drm;
pub mod headless;
pub mod pacing;
pub mod winit;

/// Which backend a compositor runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// In a window of another compositor.
    Nested,
    /// On the screen itself, from a virtual terminal.
    Drm,
    /// Without a display, on a GPU render node. For tests: nobody sees it, grim does.
    Headless,
}
