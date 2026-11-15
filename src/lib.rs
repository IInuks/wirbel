//! wirbel: a minimal Wayland compositor with a dwindle layout.

pub mod actions;
pub mod app;
pub mod backend;
pub mod check;
pub mod cli;
pub mod config;
pub mod cursor;
pub mod extension;
pub mod focus;
pub mod grabs;
pub mod handlers;
pub mod inject;
pub mod input;
pub mod input_filter;
pub mod ipc;
pub mod layout;
pub mod macros;
pub mod monitor;
pub mod plugin;
pub mod plugin_api;
pub mod plugin_input;
pub mod render;
pub mod safety;
pub mod scene_override;
pub mod state;
pub mod stats;
#[cfg(feature = "test-harness")]
pub mod test_harness;
pub mod vwm;
pub mod window_hook;
pub mod workspace;
pub mod xwayland;

pub use state::{ClientState, Wirbel};
