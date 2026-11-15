//! Invisible windows. A program started with `wirbel vwm open` runs in a shadow: a second wirbel
//! process without a display ([`shadow`]) that the visible compositor starts and watches
//! ([`manager`]). Nothing of it shows on the screen, and it has a focus, a clipboard and an X
//! server of its own, so driving it never disturbs the visible desktop. The commands are in
//! [`cli`]. `docs/vwm/DESIGN.md` has the whole picture.
//!
//! On the control socket the visible compositor answers `vwm ...` requests and a shadow answers
//! `shadow ...` requests; everything else a shadow understands is what any wirbel understands
//! (`inject`, `windows`, `subscribe`).

pub mod cli;
pub mod manager;
pub mod shadow;

use crate::Wirbel;

/// The size of a shadow's output when nobody names one.
pub const DEFAULT_SIZE: (i32, i32) = (1280, 800);

/// Whether `name` can name an invisible window: letters, digits, `-` and `_`, at most 32.
pub fn valid_name(name: &str) -> bool {
    let fits = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    !name.is_empty() && name.len() <= 32 && name.chars().all(fits) && name != "all"
}

/// A `vwm` or `shadow` request on the control socket. `None` if the line is neither. An answer
/// of `None` inside means the request answers later by itself.
pub(crate) fn request(state: &mut Wirbel, peer: u64, line: &str) -> Option<Option<String>> {
    let line = line.trim_end_matches(['\n', '\r']);
    let (word, rest) = line.split_once([' ', '\t']).unwrap_or((line, ""));
    let answer = match word {
        "vwm" => manager::request(state, peer, rest),
        "shadow" => shadow::request(state, rest).map(Some),
        _ => return None,
    };
    Some(match answer {
        Ok(answer) => answer,
        Err(err) => Some(format!("error {err}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("browser1"));
        assert!(valid_name("a-b_c"));
        assert!(!valid_name(""));
        assert!(!valid_name("all"));
        assert!(!valid_name("two words"));
        assert!(!valid_name("tab\tname"));
        assert!(!valid_name(&"x".repeat(33)));
    }
}
