//! wirbel: a minimal Wayland compositor with a dwindle layout.

pub mod backend;
mod handlers;
pub mod input;
pub mod layout;
pub mod render;
pub mod state;

pub use state::{ClientState, Wirbel};
