//! wirbel: a minimal Wayland compositor with a dwindle layout.

pub mod actions;
pub mod backend;
pub mod check;
pub mod cli;
pub mod config;
pub mod cursor;
pub mod focus;
pub mod grabs;
pub mod handlers;
pub mod input;
pub mod ipc;
pub mod layout;
pub mod monitor;
pub mod render;
pub mod safety;
pub mod state;
pub mod stats;
pub mod workspace;
pub mod xwayland;

pub use state::{ClientState, Wirbel};
