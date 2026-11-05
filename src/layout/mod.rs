//! Window layout. Free of compositor types so it can be tested on its own.

mod direction;
mod dwindle;

pub use direction::{Direction, find_in_direction};
pub use dwindle::{Dwindle, Gaps, Rect};
