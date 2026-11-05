//! Which mode an output runs in, and where it goes. Free of DRM types, so that the decision can be
//! tested and `wirbel check` can show it without touching a device.

use crate::config::MonitorRule;

/// Refresh rates this close to the rule count as a match: 164.96 Hz is what "165" means.
const REFRESH_TOLERANCE_MHZ: i32 = 500;

/// One mode an output offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeInfo {
    pub width: i32,
    pub height: i32,
    pub refresh_mhz: i32,
    pub preferred: bool,
    pub interlaced: bool,
}

/// The mode to use and, if that is not what the rule asks for, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub index: usize,
    pub warning: Option<String>,
}

/// The fastest mode of a size. Monitors tend to prefer 60 Hz whatever they can do.
fn fastest(modes: &[ModeInfo], width: i32, height: i32) -> Option<usize> {
    let of_size = |(_, mode): &(usize, &ModeInfo)| {
        mode.width == width && mode.height == height && !mode.interlaced
    };
    let candidates = modes.iter().enumerate().filter(of_size);
    // Ties go to the first entry, which is how the kernel sorts the better timing.
    candidates
        .rev()
        .max_by_key(|(_, mode)| mode.refresh_mhz)
        .map(|(index, _)| index)
}

/// Without a rule: the preferred size at its highest refresh rate.
fn default_mode(modes: &[ModeInfo]) -> Option<usize> {
    let preferred = modes.iter().find(|mode| mode.preferred).or(modes.first())?;
    fastest(modes, preferred.width, preferred.height).or(Some(0))
}

/// Picks the mode for an output. A rule that cannot be met must not cost the screen, so it ends in
/// the default with a warning. `None` if the output has no modes at all.
pub fn pick_mode(modes: &[ModeInfo], rule: Option<&MonitorRule>) -> Option<Choice> {
    let fallback = |warning| {
        default_mode(modes).map(|index| Choice {
            index,
            warning: Some(warning),
        })
    };
    let Some(rule) = rule else {
        return default_mode(modes).map(|index| Choice {
            index,
            warning: None,
        });
    };
    let Some((width, height)) = rule.size else {
        return match rule.refresh {
            None => default_mode(modes).map(|index| Choice {
                index,
                warning: None,
            }),
            Some(_) => fallback("the rule gives a refresh rate without a size".to_string()),
        };
    };
    let Some(best) = fastest(modes, width, height) else {
        return fallback(format!("no mode of size {width}x{height}"));
    };
    let Some(refresh) = rule.refresh else {
        return Some(Choice {
            index: best,
            warning: None,
        });
    };
    let wanted = (refresh * 1000.0).round() as i32;
    let distance = |mode: &ModeInfo| (mode.refresh_mhz - wanted).abs();
    let nearest = modes
        .iter()
        .enumerate()
        .filter(|(_, mode)| mode.width == width && mode.height == height && !mode.interlaced)
        .min_by_key(|(_, mode)| distance(mode));
    match nearest {
        Some((index, mode)) if distance(mode) <= REFRESH_TOLERANCE_MHZ => Some(Choice {
            index,
            warning: None,
        }),
        _ => Some(Choice {
            index: best,
            warning: Some(format!("no mode of {width}x{height} at {refresh} Hz")),
        }),
    }
}

/// Where a new output goes: the rule says so, or else right of everything there is.
pub fn position_of(
    rule: Option<&MonitorRule>,
    occupied: impl IntoIterator<Item = (i32, i32)>,
) -> (i32, i32) {
    if let Some((x, y)) = rule.and_then(|rule| rule.position) {
        return (x, y);
    }
    let right = occupied.into_iter().map(|(x, width)| x + width).max();
    (right.unwrap_or(0), 0)
}

/// Who a monitor says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub make: String,
    pub model: String,
    pub serial: String,
}

impl Identity {
    /// Reads the base block of an EDID. Works with whatever is there: some cables and most KVM
    /// switches give nothing at all. libdisplay-info would be overkill and differs between strata.
    pub fn from_edid(edid: &[u8]) -> Self {
        let unknown = || "Unknown".to_string();
        let mut identity = Self {
            make: unknown(),
            model: unknown(),
            serial: unknown(),
        };
        if edid.len() < 128 || edid[..8] != [0, 255, 255, 255, 255, 255, 255, 0] {
            return identity;
        }
        let id = u16::from_be_bytes([edid[8], edid[9]]);
        let letter = |shift: u16| char::from(b'@' + ((id >> shift) & 0x1f) as u8);
        identity.make = [letter(10), letter(5), letter(0)].iter().collect();
        for descriptor in edid[54..126].chunks_exact(18) {
            let [0, 0, 0, tag, 0, text @ ..] = descriptor else {
                continue;
            };
            let end = text.iter().position(|byte| *byte == b'\n');
            let text = String::from_utf8_lossy(&text[..end.unwrap_or(text.len())]);
            match tag {
                0xfc => identity.model = text.trim().to_string(),
                0xff => identity.serial = text.trim().to_string(),
                _ => {}
            }
        }
        identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_comes_from_the_edid() {
        let mut edid = vec![0, 255, 255, 255, 255, 255, 255, 0, 0x21, 0x63];
        edid.resize(54, 0);
        let mut descriptor = |tag: u8, text: &[u8]| {
            edid.extend_from_slice(&[0, 0, 0, tag, 0]);
            let mut text = text.to_vec();
            text.resize(13, 0x20);
            edid.extend_from_slice(&text);
        };
        descriptor(0xfd, b"");
        descriptor(0xfc, b"GN07\n");
        descriptor(0xff, b"0000\n");
        edid.resize(128, 0);
        let identity = Identity::from_edid(&edid);
        assert_eq!(identity.make, "HKC");
        assert_eq!(identity.model, "GN07");
        assert_eq!(identity.serial, "0000");
    }

    #[test]
    fn garbage_edid_is_unknown() {
        assert_eq!(Identity::from_edid(&[1, 2, 3]).make, "Unknown");
    }

    fn mode(width: i32, height: i32, refresh_mhz: i32) -> ModeInfo {
        ModeInfo {
            width,
            height,
            refresh_mhz,
            preferred: false,
            interlaced: false,
        }
    }

    /// Roughly what the monitor of this machine offers: the first mode is the preferred 60 Hz.
    fn modes() -> Vec<ModeInfo> {
        let mut preferred = mode(2560, 1440, 59_951);
        preferred.preferred = true;
        vec![
            preferred,
            mode(3840, 2160, 60_000),
            mode(2560, 1440, 169_996),
            mode(2560, 1440, 164_956),
            mode(2560, 1440, 143_973),
            mode(1920, 1080, 60_000),
        ]
    }

    fn rule(size: Option<(i32, i32)>, refresh: Option<f64>) -> MonitorRule {
        MonitorRule {
            name: "DP-4".into(),
            size,
            refresh,
            scale: None,
            position: None,
        }
    }

    #[test]
    fn rule_finds_165_hz_in_a_fractional_mode() {
        let rule = rule(Some((2560, 1440)), Some(165.0));
        let choice = pick_mode(&modes(), Some(&rule)).unwrap();
        assert_eq!((choice.index, choice.warning), (3, None));
    }

    #[test]
    fn rule_without_refresh_takes_the_fastest_of_that_size() {
        let rule = rule(Some((1920, 1080)), None);
        assert_eq!(pick_mode(&modes(), Some(&rule)).unwrap().index, 5);
    }

    #[test]
    fn missing_refresh_keeps_the_size_and_warns() {
        let rule = rule(Some((2560, 1440)), Some(240.0));
        let choice = pick_mode(&modes(), Some(&rule)).unwrap();
        assert_eq!(choice.index, 2);
        assert!(choice.warning.unwrap().contains("240"));
    }

    #[test]
    fn missing_size_falls_back_and_warns() {
        let rule = rule(Some((1024, 768)), Some(60.0));
        let choice = pick_mode(&modes(), Some(&rule)).unwrap();
        assert_eq!(choice.index, 2);
        assert!(choice.warning.unwrap().contains("1024x768"));
    }

    #[test]
    fn no_rule_means_preferred_size_at_full_speed() {
        assert_eq!(pick_mode(&modes(), None).unwrap().index, 2);
    }

    #[test]
    fn interlaced_modes_are_passed_over() {
        let mut fast = mode(1920, 1080, 120_000);
        fast.interlaced = true;
        let mut slow = mode(1920, 1080, 60_000);
        slow.preferred = true;
        assert_eq!(pick_mode(&[fast, slow], None).unwrap().index, 1);
    }

    #[test]
    fn no_modes_no_choice() {
        assert_eq!(pick_mode(&[], None), None);
    }

    #[test]
    fn first_of_equal_modes_wins() {
        let modes = [mode(800, 600, 60_000), mode(800, 600, 60_000)];
        assert_eq!(pick_mode(&modes, None).unwrap().index, 0);
    }

    #[test]
    fn outputs_line_up_unless_the_rule_says_otherwise() {
        assert_eq!(position_of(None, []), (0, 0));
        assert_eq!(position_of(None, [(0, 2560), (2560, 1920)]), (4480, 0));
        let mut placed = rule(None, None);
        placed.position = Some((-1920, 100));
        assert_eq!(position_of(Some(&placed), [(0, 2560)]), (-1920, 100));
    }
}
