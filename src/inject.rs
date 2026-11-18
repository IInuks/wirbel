//! Scripted input for unattended tests: `inject ...` on the control socket and the input scripts
//! of `wirbel inject --script`.
//!
//! Injected events take the paths of real ones (`on_key`, `on_pointer_delta`, `on_pointer_button`,
//! `on_pointer_axis`), so binds, grabs, focus and clients see them like a keyboard and a mouse.
//! The compositor only accepts them with `--test-input`, which `--headless` implies: a program in
//! the user's session must not be able to type into their windows.
//!
//! Requests, one per line:
//!
//! - `inject key NAME down|up` presses or releases one key.
//! - `inject tap NAME` presses and releases it, with Shift around it if the keymap needs that
//!   (`tap E` types a capital E).
//! - `inject chord MODS+KEY` holds the modifiers, taps the key and lets go, like a bind:
//!   `chord SUPER+SHIFT+Escape`. The key is read like the key of a bind.
//! - `inject type TEXT` types the rest of the line.
//! - `inject motion DX DY` moves the mouse by that much, like a relative libinput motion.
//! - `inject at X Y` puts the pointer at that place of the layout, like a tablet or a touchpad
//!   in absolute mode would.
//! - `inject button left|right|middle down|up`.
//! - `inject wheel STEPS` turns the wheel by that many notches, positive is down.
//!
//! Each answers `ok` or `error MESSAGE`. A key NAME is one character or an xkb keysym name such
//! as `Return`, `space` or `Escape`.

use std::time::Duration;

use smithay::{
    backend::input::{AxisSource, ButtonState, InputTime, KeyState, Keycode},
    input::keyboard::{
        Keysym, keysyms,
        xkb::{self, KEYSYM_CASE_INSENSITIVE, KEYSYM_NO_FLAGS},
    },
};

use crate::{
    Wirbel,
    config::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Modifiers, parse_key, parse_modifiers},
    input::Scroll,
};

/// xkb codes are evdev codes shifted by this.
const XKB_OFFSET: u32 = 8;
/// The evdev codes of the left modifier keys, which every keymap has in their usual places.
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTALT: u32 = 56;
const KEY_LEFTMETA: u32 = 125;
const KEY_RIGHTALT: u32 = 100;
/// What one wheel notch scrolls, as libinput reports it.
const NOTCH: f64 = 15.0;

/// One injected input event, or a few that belong together.
#[derive(Debug, Clone, PartialEq)]
pub enum Inject {
    Key { key: Keysym, down: bool },
    Tap(Keysym),
    Chord { modifiers: Modifiers, key: Keysym },
    Type(String),
    Motion { dx: f64, dy: f64 },
    At { x: f64, y: f64 },
    Button { button: u32, down: bool },
    Wheel(f64),
}

/// Letters with an accent, by the dead key that makes them: each accented one lines up
/// with the plain one at the same place.
const ACCENTED: [(u32, &str, &str); 5] = [
    (keysyms::KEY_dead_grave, "àèìòùÀÈÌÒÙ", "aeiouAEIOU"),
    (keysyms::KEY_dead_acute, "áéíóúýÁÉÍÓÚÝ", "aeiouyAEIOUY"),
    (keysyms::KEY_dead_circumflex, "âêîôûÂÊÎÔÛ", "aeiouAEIOU"),
    (keysyms::KEY_dead_tilde, "ãñõÃÑÕ", "anoANO"),
    (keysyms::KEY_dead_diaeresis, "ëïÿËÏ", "eiyEI"),
];

/// The dead key and the character after it that make `c` on a layout where `c` has no key:
/// a space after the dead key gives the accent itself.
fn dead_key_for(c: char) -> Option<(u32, char)> {
    let spacing = match c {
        '^' => Some(keysyms::KEY_dead_circumflex),
        '`' => Some(keysyms::KEY_dead_grave),
        '´' => Some(keysyms::KEY_dead_acute),
        '~' => Some(keysyms::KEY_dead_tilde),
        '¨' => Some(keysyms::KEY_dead_diaeresis),
        _ => None,
    };
    if let Some(dead) = spacing {
        return Some((dead, ' '));
    }
    ACCENTED.iter().find_map(|(dead, accented, plain)| {
        let at = accented.chars().position(|a| a == c)?;
        Some((*dead, plain.chars().nth(at)?))
    })
}

/// A key name as the requests take it: one character as it is, case included, or a keysym name.
pub fn parse_keysym(name: &str) -> Result<Keysym, String> {
    let mut chars = name.chars();
    let key = match (chars.next(), chars.next()) {
        (Some(c), None) => xkb::utf32_to_keysym(c as u32),
        _ => match xkb::keysym_from_name(name, KEYSYM_NO_FLAGS) {
            key if key.raw() == keysyms::KEY_NoSymbol => {
                xkb::keysym_from_name(name, KEYSYM_CASE_INSENSITIVE)
            }
            key => key,
        },
    };
    if key.raw() == keysyms::KEY_NoSymbol {
        return Err(format!("unknown key `{name}`"));
    }
    Ok(key)
}

fn keysym_name(key: Keysym) -> String {
    xkb::keysym_get_name(key)
}

fn parse_button_name(name: &str) -> Result<u32, String> {
    match name {
        "left" => Ok(BTN_LEFT),
        "right" => Ok(BTN_RIGHT),
        "middle" => Ok(BTN_MIDDLE),
        _ => Err(format!(
            "unknown button `{name}`, expected left, right or middle"
        )),
    }
}

fn button_name(button: u32) -> &'static str {
    match button {
        BTN_RIGHT => "right",
        BTN_MIDDLE => "middle",
        _ => "left",
    }
}

fn parse_direction(word: &str) -> Result<bool, String> {
    match word {
        "down" => Ok(true),
        "up" => Ok(false),
        _ => Err(format!("expected down or up, got `{word}`")),
    }
}

fn parse_number(word: &str) -> Result<f64, String> {
    let value: f64 = word
        .parse()
        .map_err(|_| format!("expected a number, got `{word}`"))?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("not a finite number: {word}"))
    }
}

/// `SUPER+SHIFT+Escape`: the last part is the key, the others are modifiers.
fn parse_chord(text: &str) -> Result<Inject, String> {
    let (modifiers, key) = match text.rsplit_once('+') {
        // `SUPER++` is Super and the plus key.
        Some((modifiers, "")) => (modifiers.strip_suffix('+').unwrap_or(modifiers), "plus"),
        Some((modifiers, key)) => (modifiers, key),
        None => ("none", text),
    };
    Ok(Inject::Chord {
        modifiers: parse_modifiers(modifiers)?,
        key: parse_key(key)?,
    })
}

fn modifiers_text(modifiers: Modifiers) -> String {
    let names = [
        (modifiers.logo, "SUPER"),
        (modifiers.ctrl, "CTRL"),
        (modifiers.alt, "ALT"),
        (modifiers.shift, "SHIFT"),
        (modifiers.altgr, "ALTGR"),
    ];
    let held: Vec<&str> = names
        .iter()
        .filter(|(held, _)| *held)
        .map(|(_, name)| *name)
        .collect();
    if held.is_empty() {
        "NONE".into()
    } else {
        held.join("+")
    }
}

/// The evdev codes of the keys that hold `modifiers`, in the order they go down.
fn modifier_keys(modifiers: Modifiers) -> Vec<u32> {
    [
        (modifiers.logo, KEY_LEFTMETA),
        (modifiers.ctrl, KEY_LEFTCTRL),
        (modifiers.alt, KEY_LEFTALT),
        (modifiers.shift, KEY_LEFTSHIFT),
        (modifiers.altgr, KEY_RIGHTALT),
    ]
    .into_iter()
    .filter(|(held, _)| *held)
    .map(|(_, code)| code)
    .collect()
}

impl Inject {
    pub fn to_line(&self) -> String {
        match self {
            Self::Key { key, down } => {
                let direction = if *down { "down" } else { "up" };
                format!("inject key {} {direction}", keysym_name(*key))
            }
            Self::Tap(key) => format!("inject tap {}", keysym_name(*key)),
            Self::Chord { modifiers, key } => {
                format!(
                    "inject chord {}+{}",
                    modifiers_text(*modifiers),
                    keysym_name(*key)
                )
            }
            Self::Type(text) => format!("inject type {text}"),
            Self::Motion { dx, dy } => format!("inject motion {dx:?} {dy:?}"),
            Self::At { x, y } => format!("inject at {x:?} {y:?}"),
            Self::Button { button, down } => {
                let direction = if *down { "down" } else { "up" };
                format!("inject button {} {direction}", button_name(*button))
            }
            Self::Wheel(steps) => format!("inject wheel {steps:?}"),
        }
    }

    /// `None` if the line is no `inject` request at all.
    pub fn parse(line: &str) -> Option<Result<Self, String>> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let rest = line.strip_prefix("inject")?;
        if !(rest.is_empty() || rest.starts_with(' ')) {
            return None;
        }
        Some(Self::parse_arguments(
            rest.strip_prefix(' ').unwrap_or(rest),
        ))
    }

    fn parse_arguments(text: &str) -> Result<Self, String> {
        // `type` keeps its text as it is, spaces included.
        if let Some(text) = text.strip_prefix("type ") {
            if text.is_empty() {
                return Err("nothing to type".into());
            }
            return Ok(Self::Type(text.into()));
        }
        let words: Vec<&str> = text.split_ascii_whitespace().collect();
        Ok(match words.as_slice() {
            ["key", name, direction] => Self::Key {
                key: parse_keysym(name)?,
                down: parse_direction(direction)?,
            },
            ["tap", name] => Self::Tap(parse_keysym(name)?),
            ["chord", chord] => parse_chord(chord)?,
            ["motion", dx, dy] => Self::Motion {
                dx: parse_number(dx)?,
                dy: parse_number(dy)?,
            },
            ["at", x, y] => Self::At {
                x: parse_number(x)?,
                y: parse_number(y)?,
            },
            ["button", name, direction] => Self::Button {
                button: parse_button_name(name)?,
                down: parse_direction(direction)?,
            },
            ["wheel", steps] => Self::Wheel(parse_number(steps)?),
            _ => {
                return Err(
                    "expected inject key NAME down|up, tap NAME, chord MODS+KEY, type TEXT, \
                     motion DX DY, at X Y, button left|right|middle down|up or wheel STEPS"
                        .into(),
                );
            }
        })
    }
}

impl Wirbel {
    /// Plays one request. The whole of it is checked before anything is pressed, so a typo in
    /// `type` does not leave half a word behind.
    pub fn inject(&mut self, request: Inject) -> Result<(), String> {
        if !self.test_input {
            return Err("input injection is off, wirbel needs --test-input (or --headless)".into());
        }
        match request {
            Inject::Key { key, down } => {
                let (code, _) = self.key_for(key)?;
                let state = if down {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                self.on_key(code, state, InputTime::now());
            }
            Inject::Tap(key) => {
                let key = self.key_for(key)?;
                self.type_key(key);
            }
            Inject::Chord { modifiers, key } => {
                let (code, _) = self.key_for(key)?;
                let held = modifier_keys(modifiers);
                for evdev in &held {
                    self.press_evdev(*evdev, KeyState::Pressed);
                }
                self.on_key(code, KeyState::Pressed, InputTime::now());
                self.on_key(code, KeyState::Released, InputTime::now());
                for evdev in held.iter().rev() {
                    self.press_evdev(*evdev, KeyState::Released);
                }
            }
            Inject::Type(text) => {
                let mut keys = Vec::new();
                for c in text.chars() {
                    let key = match c {
                        '\n' => Keysym::Return,
                        '\t' => Keysym::Tab,
                        c => xkb::utf32_to_keysym(c as u32),
                    };
                    if let Ok(key) = self.key_for(key) {
                        keys.push(key);
                        continue;
                    }
                    // No key of its own: a dead key and what follows it, as a hand types it on
                    // a layout with dead keys. The program composes the two.
                    let (dead, then) = dead_key_for(c)
                        .ok_or_else(|| format!("cannot type `{c}` with this keymap, `paste` can put it in"))?;
                    let dead = self.key_for(Keysym::from(dead));
                    let then = self.key_for(xkb::utf32_to_keysym(then as u32));
                    match (dead, then) {
                        (Ok(dead), Ok(then)) => keys.extend([dead, then]),
                        _ => return Err(format!("cannot type `{c}` with this keymap, `paste` can put it in")),
                    }
                }
                for key in keys {
                    self.type_key(key);
                }
            }
            Inject::Motion { dx, dy } => {
                let delta = (dx, dy).into();
                self.on_pointer_delta(delta, delta, InputTime::now());
            }
            Inject::At { x, y } => {
                let location = self.clamp_to_outputs((x, y).into());
                self.on_pointer_absolute(location, InputTime::now());
            }
            Inject::Button { button, down } => {
                let state = if down {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                self.on_pointer_button(button, state, InputTime::now());
            }
            Inject::Wheel(steps) => {
                let mut scroll = Scroll::new(AxisSource::Wheel, InputTime::now());
                scroll.set(
                    smithay::backend::input::Axis::Vertical,
                    Some(steps * NOTCH),
                    Some(steps * 120.0),
                );
                self.on_pointer_axis(scroll);
            }
        }
        Ok(())
    }

    /// The key that makes `keysym` in the active layout, and the level it is on: 0 plain,
    /// 1 with Shift, 2 with AltGr, 3 with both, as the usual four-level keys have it.
    fn key_for(&mut self, keysym: Keysym) -> Result<(Keycode, u32), String> {
        let keyboard = self.seat.get_keyboard().ok_or("no keyboard")?;
        let found = keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            let layout = xkb.active_layout().0;
            // SAFETY: the reference lives only as long as the lock on the `Xkb`.
            let keymap = unsafe { xkb.keymap() };
            let mut best: Option<(Keycode, u32)> = None;
            for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
                let code = Keycode::new(raw);
                let levels = keymap.num_levels_for_key(code, layout).min(4);
                let level = (0..levels).find(|level| {
                    keymap
                        .key_get_syms_by_level(code, layout, *level)
                        .contains(&keysym)
                });
                if let Some(level) = level
                    && best.is_none_or(|(_, best)| level < best)
                {
                    best = Some((code, level));
                }
            }
            best
        });
        let (code, level) = found
            .ok_or_else(|| format!("no key makes `{}` in this keymap", keysym_name(keysym)))?;
        Ok((code, level))
    }

    fn press_evdev(&mut self, evdev: u32, state: KeyState) {
        self.on_key((evdev + XKB_OFFSET).into(), state, InputTime::now());
    }

    fn type_key(&mut self, (code, level): (Keycode, u32)) {
        let held: Vec<u32> = [
            (level & 1 != 0, KEY_LEFTSHIFT),
            (level & 2 != 0, KEY_RIGHTALT),
        ]
        .into_iter()
        .filter(|(needed, _)| *needed)
        .map(|(_, key)| key)
        .collect();
        for key in &held {
            self.press_evdev(*key, KeyState::Pressed);
        }
        self.on_key(code, KeyState::Pressed, InputTime::now());
        self.on_key(code, KeyState::Released, InputTime::now());
        for key in held.iter().rev() {
            self.press_evdev(*key, KeyState::Released);
        }
    }
}

/// One thing an input script does: send a request, or wait.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Send(Inject),
    Wait(Duration),
}

/// Between the steps of a script, so clients see them one after another, as from a hand.
pub const STEP_GAP: Duration = Duration::from_millis(20);
/// How long `btn:NAME` holds the button. Something that looks once per frame has to see it down.
pub const CLICK_HOLD: Duration = Duration::from_millis(30);
/// How often a slow `mouse:` step moves the mouse.
pub const MOTION_INTERVAL: Duration = Duration::from_millis(10);

/// The step names of a script. Text after a comma that does not start with one of them belongs
/// to the step before, so `mouse:200,0` and `type:a, b` work.
const STEP_NAMES: [&str; 9] = [
    "key", "hold", "tap", "chord", "mouse", "btn", "wheel", "wait", "type",
];

pub const SCRIPT_HELP: &str = "An input script is a list of steps separated by commas:
  key:NAME:down|up   press or release a key       hold:NAME:TIME  hold a key for TIME
  tap:NAME           press and release a key      chord:MODS+KEY  a bind, e.g. chord:SUPER+SHIFT+Escape
  type:TEXT          type TEXT                    wait:TIME       do nothing for TIME
  mouse:DX,DY[:TIME] move the mouse by DX,DY, spread over TIME if given
  btn:left|right|middle[:down|up]  click, or press or release only
  wheel:STEPS        turn the wheel, positive is down
NAME is one character (case matters, `tap:E` types a capital E) or an xkb keysym name such as
Return, space, Escape. TIME is 2s, 500ms or 1.5 (seconds). A comma in TEXT works unless a step
name and a colon follow it.";

/// `2s`, `500ms` or `1.5`.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let error = || format!("expected a time such as 2s, 500ms or 1.5, got `{text}`");
    let (number, scale) = if let Some(number) = text.strip_suffix("ms") {
        (number, 0.001)
    } else if let Some(number) = text.strip_suffix('s') {
        (number, 1.0)
    } else {
        (text, 1.0)
    };
    let value: f64 = number.parse().map_err(|_| error())?;
    Duration::try_from_secs_f64(value * scale).map_err(|_| error())
}

fn starts_step(token: &str) -> bool {
    token
        .split_once(':')
        .is_some_and(|(name, _)| STEP_NAMES.contains(&name))
}

/// Turns a script into what to send and when.
pub fn parse_script(script: &str) -> Result<Vec<Step>, String> {
    let mut items: Vec<String> = Vec::new();
    for token in script.split(',') {
        match items.last_mut() {
            Some(last) if !starts_step(token) => {
                last.push(',');
                last.push_str(token);
            }
            _ => items.push(token.to_string()),
        }
    }
    let mut steps = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            steps.push(Step::Wait(STEP_GAP));
        }
        let expanded = parse_step(item).map_err(|err| format!("step `{item}`: {err}"))?;
        steps.extend(expanded);
    }
    Ok(steps)
}

fn parse_step(item: &str) -> Result<Vec<Step>, String> {
    let (name, argument) = item
        .split_once(':')
        .ok_or("expected NAME:ARGUMENT, see --help")?;
    let send = |inject| Step::Send(inject);
    Ok(match name {
        "key" => {
            let (key, direction) = argument
                .rsplit_once(':')
                .ok_or("expected key:NAME:down|up")?;
            vec![send(Inject::Key {
                key: parse_keysym(key)?,
                down: parse_direction(direction)?,
            })]
        }
        "hold" => {
            let (key, time) = argument.rsplit_once(':').ok_or("expected hold:NAME:TIME")?;
            let key = parse_keysym(key)?;
            vec![
                send(Inject::Key { key, down: true }),
                Step::Wait(parse_duration(time)?),
                send(Inject::Key { key, down: false }),
            ]
        }
        "tap" => vec![send(Inject::Tap(parse_keysym(argument)?))],
        "chord" => vec![send(parse_chord(argument)?)],
        "type" => {
            if argument.is_empty() || argument.contains('\n') {
                return Err("expected text without line breaks, use tap:Return".into());
            }
            vec![send(Inject::Type(argument.into()))]
        }
        "wait" => vec![Step::Wait(parse_duration(argument)?)],
        "wheel" => vec![send(Inject::Wheel(parse_number(argument)?))],
        "btn" => {
            let (button, direction) = match argument.split_once(':') {
                Some((button, direction)) => (button, Some(parse_direction(direction)?)),
                None => (argument, None),
            };
            let button = parse_button_name(button)?;
            match direction {
                Some(down) => vec![send(Inject::Button { button, down })],
                None => vec![
                    send(Inject::Button { button, down: true }),
                    Step::Wait(CLICK_HOLD),
                    send(Inject::Button {
                        button,
                        down: false,
                    }),
                ],
            }
        }
        "mouse" => {
            let (delta, time) = match argument.split_once(':') {
                Some((delta, time)) => (delta, parse_duration(time)?),
                None => (argument, Duration::ZERO),
            };
            let (dx, dy) = delta.split_once(',').ok_or("expected mouse:DX,DY[:TIME]")?;
            let (dx, dy) = (parse_number(dx)?, parse_number(dy)?);
            // Even parts at an even pace; the last one ends where one move would have.
            let parts = (time.as_secs_f64() / MOTION_INTERVAL.as_secs_f64())
                .round()
                .max(1.0) as u32;
            let mut steps = Vec::new();
            for part in 0..parts {
                if part > 0 {
                    steps.push(Step::Wait(MOTION_INTERVAL));
                }
                steps.push(send(Inject::Motion {
                    dx: dx / f64::from(parts),
                    dy: dy / f64::from(parts),
                }));
            }
            steps
        }
        _ => return Err(format!("unknown step `{name}`, see --help")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> Keysym {
        parse_keysym(name).unwrap()
    }

    #[test]
    fn characters_without_a_key_go_through_dead_keys() {
        assert_eq!(dead_key_for('^'), Some((keysyms::KEY_dead_circumflex, ' ')));
        assert_eq!(dead_key_for('é'), Some((keysyms::KEY_dead_acute, 'e')));
        assert_eq!(dead_key_for('Ñ'), Some((keysyms::KEY_dead_tilde, 'N')));
        assert_eq!(dead_key_for('Û'), Some((keysyms::KEY_dead_circumflex, 'U')));
        assert_eq!(dead_key_for('x'), None);
        for (_, accented, plain) in ACCENTED {
            assert_eq!(accented.chars().count(), plain.chars().count());
        }
    }

    #[test]
    fn requests_survive_the_wire() {
        let super_shift = Modifiers {
            logo: true,
            shift: true,
            ..Default::default()
        };
        for request in [
            Inject::Key {
                key: key("w"),
                down: true,
            },
            Inject::Key {
                key: key("Return"),
                down: false,
            },
            Inject::Tap(key("E")),
            Inject::Tap(key("space")),
            Inject::Tap(key("ß")),
            Inject::Chord {
                modifiers: super_shift,
                key: key("Escape"),
            },
            Inject::Chord {
                modifiers: Modifiers::default(),
                key: key("a"),
            },
            Inject::Type("echo  hi, there".into()),
            Inject::Motion {
                dx: 200.0,
                dy: -0.5,
            },
            Inject::Button {
                button: BTN_RIGHT,
                down: true,
            },
            Inject::Button {
                button: BTN_MIDDLE,
                down: false,
            },
            Inject::Wheel(-3.0),
        ] {
            let line = request.to_line();
            assert_eq!(Inject::parse(&line), Some(Ok(request)), "{line}");
            assert!(Inject::parse(&(line + "\n")).unwrap().is_ok());
        }
    }

    #[test]
    fn other_lines_are_not_injections() {
        assert_eq!(Inject::parse("position"), None);
        assert_eq!(Inject::parse("injected"), None);
    }

    #[test]
    fn bad_requests_are_refused() {
        for line in [
            "inject",
            "inject dance",
            "inject key w",
            "inject key w sideways",
            "inject key NoSuchKey down",
            "inject tap",
            "inject tap a b",
            "inject chord FOO+a",
            "inject type ",
            "inject motion 1",
            "inject motion 1 NaN",
            "inject button left",
            "inject button back down",
            "inject wheel",
            "inject wheel inf",
        ] {
            assert!(
                matches!(Inject::parse(line), Some(Err(_))),
                "accepted {line:?}"
            );
        }
    }

    #[test]
    fn single_characters_keep_their_case() {
        assert_eq!(key("e").raw(), keysyms::KEY_e);
        assert_eq!(key("E").raw(), keysyms::KEY_E);
        assert_eq!(key("Return").raw(), keysyms::KEY_Return);
        assert_eq!(key("escape").raw(), keysyms::KEY_Escape);
    }

    #[test]
    fn chords_read_like_binds() {
        let Ok(Inject::Chord { modifiers, key }) = parse_chord("SUPER+space") else {
            panic!();
        };
        assert!(modifiers.logo && !modifiers.shift);
        assert_eq!(key.raw(), keysyms::KEY_space);
        // Like a bind, a letter is the unshifted key.
        let Ok(Inject::Chord { modifiers, key }) = parse_chord("Super+Shift+E") else {
            panic!();
        };
        assert!(modifiers.logo && modifiers.shift);
        assert_eq!(key.raw(), keysyms::KEY_e);
        let Ok(Inject::Chord { key, .. }) = parse_chord("CTRL++") else {
            panic!();
        };
        assert_eq!(key.raw(), keysyms::KEY_plus);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("2s"), Ok(Duration::from_secs(2)));
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("1.5"), Ok(Duration::from_millis(1500)));
        assert!(parse_duration("-1s").is_err());
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn the_example_script() {
        let script =
            "hold:w:2s,tap:e,chord:SUPER+space,mouse:200,0,btn:right,wheel:3,wait:1s,type:text";
        let w = key("w");
        let gap = Step::Wait(STEP_GAP);
        let right = |down| {
            Step::Send(Inject::Button {
                button: BTN_RIGHT,
                down,
            })
        };
        let expected = vec![
            Step::Send(Inject::Key { key: w, down: true }),
            Step::Wait(Duration::from_secs(2)),
            Step::Send(Inject::Key {
                key: w,
                down: false,
            }),
            gap.clone(),
            Step::Send(Inject::Tap(key("e"))),
            gap.clone(),
            Step::Send(Inject::Chord {
                modifiers: Modifiers {
                    logo: true,
                    ..Default::default()
                },
                key: key("space"),
            }),
            gap.clone(),
            Step::Send(Inject::Motion { dx: 200.0, dy: 0.0 }),
            gap.clone(),
            right(true),
            Step::Wait(CLICK_HOLD),
            right(false),
            gap.clone(),
            Step::Send(Inject::Wheel(3.0)),
            gap.clone(),
            Step::Wait(Duration::from_secs(1)),
            gap,
            Step::Send(Inject::Type("text".into())),
        ];
        assert_eq!(parse_script(script), Ok(expected));
    }

    #[test]
    fn commas_belong_to_text_unless_a_step_follows() {
        let steps = parse_script("type:echo a, b,c,tap:Return").unwrap();
        assert_eq!(steps[0], Step::Send(Inject::Type("echo a, b,c".into())));
        assert_eq!(steps[2], Step::Send(Inject::Tap(key("Return"))));
        // `x:` is no step name, so it is text too.
        let steps = parse_script("type:a,x:y").unwrap();
        assert_eq!(steps, [Step::Send(Inject::Type("a,x:y".into()))]);
    }

    #[test]
    fn slow_mouse_moves_are_spread_out() {
        let steps = parse_script("mouse:100,-50:50ms").unwrap();
        let moves: Vec<&Step> = steps
            .iter()
            .filter(|step| matches!(step, Step::Send(_)))
            .collect();
        assert_eq!(moves.len(), 5);
        assert_eq!(
            moves[0],
            &Step::Send(Inject::Motion {
                dx: 20.0,
                dy: -10.0
            })
        );
        let waits = steps
            .iter()
            .filter(|step| **step == Step::Wait(MOTION_INTERVAL));
        assert_eq!(waits.count(), 4);
    }

    #[test]
    fn button_steps() {
        let steps = parse_script("btn:left:down,btn:left:up").unwrap();
        let left = |down| {
            Step::Send(Inject::Button {
                button: BTN_LEFT,
                down,
            })
        };
        assert_eq!(steps, [left(true), Step::Wait(STEP_GAP), left(false)]);
    }

    #[test]
    fn bad_scripts_are_refused() {
        for script in [
            "",
            "dance",
            "dance:now",
            "hold:w",
            "hold:w:forever",
            "key:w",
            "key:w:sideways",
            "tap:NoSuchKey",
            "type:",
            "mouse:1",
            "mouse:1,2:soon",
            "btn:back",
            "btn:left:sideways",
            "wheel:lots",
            "wait:1s,",
        ] {
            assert!(parse_script(script).is_err(), "accepted {script:?}");
        }
    }
}
