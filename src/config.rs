//! The configuration file: `name = value` lines, plus `bind` and `mousebind`.

use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
};

use smithay::input::keyboard::{
    Keysym, KeysymHandle, ModifiersState, XkbConfig, keysyms,
    xkb::{self, KEYSYM_CASE_INSENSITIVE, KEYSYM_NO_FLAGS},
};

use crate::layout::{Direction, Gaps};

const DEFAULTS: &str = include_str!("default.conf");

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

/// Number of workspaces per output.
pub const WORKSPACES: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl From<&ModifiersState> for Modifiers {
    fn from(state: &ModifiersState) -> Self {
        Self {
            ctrl: state.ctrl,
            alt: state.alt,
            shift: state.shift,
            logo: state.logo,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTarget {
    /// Zero-based.
    Index(usize),
    Previous,
    Next,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Spawn(String),
    Close,
    Quit,
    Reload,
    Focus(Direction),
    FocusNext,
    FocusPrevious,
    Swap(Direction),
    SplitRatio(f32),
    Float,
    Fullscreen,
    Maximize,
    Workspace(WorkspaceTarget),
    /// Zero-based.
    MoveToWorkspace(usize),
    FocusOutput(Direction),
    MoveToOutput(Direction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Move,
    Resize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bind {
    pub modifiers: Modifiers,
    pub key: Keysym,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MouseBind {
    pub modifiers: Modifiers,
    pub button: u32,
    pub action: MouseAction,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub gaps: Gaps,
    pub border_width: i32,
    pub border_color: [f32; 4],
    pub focus_color: [f32; 4],
    pub background_color: [f32; 4],
    pub keyboard_layout: String,
    pub keyboard_variant: String,
    pub keyboard_options: String,
    pub repeat_delay: i32,
    pub repeat_rate: i32,
    pub sloppy_focus: bool,
    pub warp_cursor: bool,
    pub binds: Vec<Bind>,
    pub mouse_binds: Vec<MouseBind>,
}

/// A line of the configuration that could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut config = Self::empty();
        for (index, line) in DEFAULTS.lines().enumerate() {
            config
                .apply_line(line)
                .unwrap_or_else(|err| panic!("default.conf line {}: {err}", index + 1));
        }
        config
    }
}

impl Config {
    fn empty() -> Self {
        Self {
            gaps: Gaps { outer: 0, inner: 0 },
            border_width: 0,
            border_color: [0.0; 4],
            focus_color: [0.0; 4],
            background_color: [0.0; 4],
            keyboard_layout: String::new(),
            keyboard_variant: String::new(),
            keyboard_options: String::new(),
            repeat_delay: 0,
            repeat_rate: 0,
            sloppy_focus: false,
            warp_cursor: false,
            binds: Vec::new(),
            mouse_binds: Vec::new(),
        }
    }

    /// `$XDG_CONFIG_HOME/wirbel/config`, or `~/.config/wirbel/config`.
    pub fn default_path() -> Option<PathBuf> {
        let base = env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("wirbel").join("config"))
    }

    /// Reads `path`. A missing file means the built-in defaults.
    pub fn load(path: &Path) -> io::Result<(Self, Vec<ConfigError>)> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok((Self::default(), Vec::new())),
            Err(err) => Err(err),
        }
    }

    /// Parses a configuration on top of the defaults. Unusable lines are skipped and reported.
    pub fn parse(text: &str) -> (Self, Vec<ConfigError>) {
        let mut config = Self::default();
        let (default_binds, default_mouse_binds) = (
            std::mem::take(&mut config.binds),
            std::mem::take(&mut config.mouse_binds),
        );
        let mut errors = Vec::new();
        let mut has_binds = false;
        let mut has_mouse_binds = false;

        for (index, line) in text.lines().enumerate() {
            match line.trim_start().split(['=', ' ', '\t']).next() {
                Some("bind") => has_binds = true,
                Some("mousebind") => has_mouse_binds = true,
                _ => {}
            }
            if let Err(message) = config.apply_line(line) {
                errors.push(ConfigError {
                    line: index + 1,
                    message,
                });
            }
        }

        if !has_binds {
            config.binds = default_binds;
        } else if !config.binds.iter().any(|bind| bind.action == Action::Quit) {
            let quit = default_binds
                .into_iter()
                .find(|bind| bind.action == Action::Quit);
            // In front, because the first match wins and this one must always work.
            config.binds.splice(0..0, quit);
        }
        if !has_mouse_binds {
            config.mouse_binds = default_mouse_binds;
        }
        (config, errors)
    }

    fn apply_line(&mut self, line: &str) -> Result<(), String> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(());
        }
        let (name, value) = line
            .split_once('=')
            .ok_or("expected `name = value`".to_string())?;
        let value = value.trim();
        match name.trim() {
            "gap_outer" => self.gaps.outer = parse_size(value)?,
            "gap_inner" => self.gaps.inner = parse_size(value)?,
            "border_width" => self.border_width = parse_size(value)?,
            "border_color" => self.border_color = parse_color(value)?,
            "focus_color" => self.focus_color = parse_color(value)?,
            "background_color" => self.background_color = parse_color(value)?,
            "keyboard_layout" => self.keyboard_layout = value.to_string(),
            "keyboard_variant" => self.keyboard_variant = value.to_string(),
            "keyboard_options" => self.keyboard_options = value.to_string(),
            "repeat_delay" => self.repeat_delay = parse_size(value)?,
            "repeat_rate" => self.repeat_rate = parse_size(value)?,
            "sloppy_focus" => self.sloppy_focus = parse_bool(value)?,
            "warp_cursor" => self.warp_cursor = parse_bool(value)?,
            "bind" => self.binds.push(parse_bind(value)?),
            "mousebind" => self.mouse_binds.push(parse_mouse_bind(value)?),
            other => return Err(format!("unknown option `{other}`")),
        }
        Ok(())
    }

    pub fn xkb(&self) -> XkbConfig<'_> {
        XkbConfig {
            layout: &self.keyboard_layout,
            variant: &self.keyboard_variant,
            options: (!self.keyboard_options.is_empty()).then(|| self.keyboard_options.clone()),
            ..XkbConfig::default()
        }
    }

    /// Whether switching to `other` needs a new keymap.
    pub fn same_keymap(&self, other: &Self) -> bool {
        self.keyboard_layout == other.keyboard_layout
            && self.keyboard_variant == other.keyboard_variant
            && self.keyboard_options == other.keyboard_options
    }

    /// The action bound to a key press. Keys match by the symbol they produce with the active
    /// modifiers or by their unshifted symbol, so `SUPER+SHIFT, 1` does not depend on what the
    /// layout puts on Shift+1.
    pub fn action_for_key(
        &self,
        modifiers: Modifiers,
        handle: &KeysymHandle<'_>,
    ) -> Option<&Action> {
        let produced = handle.modified_syms();
        let raw = handle.raw_syms();
        self.binds
            .iter()
            .find(|bind| {
                bind.modifiers == modifiers
                    && (produced.contains(&bind.key) || raw.contains(&bind.key))
            })
            .map(|bind| &bind.action)
    }

    pub fn action_for_button(&self, modifiers: Modifiers, button: u32) -> Option<MouseAction> {
        self.mouse_binds
            .iter()
            .find(|bind| bind.modifiers == modifiers && bind.button == button)
            .map(|bind| bind.action)
    }
}

fn parse_size(value: &str) -> Result<i32, String> {
    value
        .parse::<u16>()
        .map(i32::from)
        .map_err(|_| format!("expected a number from 0 to 65535, got `{value}`"))
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("expected 0 or 1, got `{value}`")),
    }
}

/// Returns premultiplied RGBA, which is what the renderer wants.
fn parse_color(value: &str) -> Result<[f32; 4], String> {
    let error = || format!("expected a colour like 0xRRGGBB, got `{value}`");
    let digits = value
        .strip_prefix("0x")
        .or(value.strip_prefix('#'))
        .ok_or_else(error)?;
    let number = u32::from_str_radix(digits, 16).map_err(|_| error())?;
    let [r, g, b, a] = match digits.len() {
        6 => (number << 8 | 0xff).to_be_bytes(),
        8 => number.to_be_bytes(),
        _ => return Err(error()),
    };
    let alpha = f32::from(a) / 255.0;
    Ok([
        f32::from(r) / 255.0 * alpha,
        f32::from(g) / 255.0 * alpha,
        f32::from(b) / 255.0 * alpha,
        alpha,
    ])
}

fn parse_modifiers(value: &str) -> Result<Modifiers, String> {
    let mut modifiers = Modifiers::default();
    for name in value.split('+').map(str::trim) {
        match name.to_ascii_lowercase().as_str() {
            "super" | "logo" | "mod4" => modifiers.logo = true,
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" | "mod1" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            "none" => {}
            _ => return Err(format!("unknown modifier `{name}`")),
        }
    }
    Ok(modifiers)
}

fn parse_key(name: &str) -> Result<Keysym, String> {
    let mut chars = name.chars();
    let key = match (chars.next(), chars.next()) {
        // Letters are stored in lower case, which is what an unshifted key produces.
        (Some(letter), None) => {
            let lower = letter.to_lowercase().next().unwrap_or(letter);
            xkb::utf32_to_keysym(lower as u32)
        }
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

fn parse_direction(value: &str) -> Result<Direction, String> {
    match value {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        _ => Err(format!("expected left, right, up or down, got `{value}`")),
    }
}

fn parse_workspace(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(number) if (1..=WORKSPACES).contains(&number) => Ok(number - 1),
        _ => Err(format!("expected a workspace from 1 to 9, got `{value}`")),
    }
}

fn parse_action(name: &str, argument: &str) -> Result<Action, String> {
    let action = match name {
        "spawn" if argument.is_empty() => return Err("spawn needs a command".into()),
        "spawn" => return Ok(Action::Spawn(argument.to_string())),
        "focus" => return parse_direction(argument).map(Action::Focus),
        "swap" => return parse_direction(argument).map(Action::Swap),
        "focus_output" => return parse_direction(argument).map(Action::FocusOutput),
        "move_to_output" => return parse_direction(argument).map(Action::MoveToOutput),
        "move_to_workspace" => return parse_workspace(argument).map(Action::MoveToWorkspace),
        "workspace" => {
            return Ok(Action::Workspace(match argument {
                "prev" => WorkspaceTarget::Previous,
                "next" => WorkspaceTarget::Next,
                _ => WorkspaceTarget::Index(parse_workspace(argument)?),
            }));
        }
        "split_ratio" => {
            let delta = argument
                .parse::<f32>()
                .ok()
                .filter(|delta| delta.is_finite())
                .ok_or(format!("expected a number like +0.05, got `{argument}`"))?;
            return Ok(Action::SplitRatio(delta));
        }
        "close" => Action::Close,
        "quit" => Action::Quit,
        "reload" => Action::Reload,
        "focus_next" => Action::FocusNext,
        "focus_prev" => Action::FocusPrevious,
        "float" => Action::Float,
        "fullscreen" => Action::Fullscreen,
        "maximize" => Action::Maximize,
        _ => return Err(format!("unknown action `{name}`")),
    };
    if argument.is_empty() {
        Ok(action)
    } else {
        Err(format!("{name} takes no argument"))
    }
}

fn parse_bind(value: &str) -> Result<Bind, String> {
    // The command of a `spawn` may contain commas.
    let mut fields = value.splitn(4, ',').map(str::trim);
    let (Some(modifiers), Some(key), Some(action)) = (fields.next(), fields.next(), fields.next())
    else {
        return Err("expected `bind = MODIFIERS, KEY, ACTION[, ARGUMENT]`".into());
    };
    Ok(Bind {
        modifiers: parse_modifiers(modifiers)?,
        key: parse_key(key)?,
        action: parse_action(action, fields.next().unwrap_or(""))?,
    })
}

fn parse_mouse_bind(value: &str) -> Result<MouseBind, String> {
    let fields: Vec<&str> = value.split(',').map(str::trim).collect();
    let &[modifiers, button, action] = fields.as_slice() else {
        return Err("expected `mousebind = MODIFIERS, BUTTON, ACTION`".into());
    };
    Ok(MouseBind {
        modifiers: parse_modifiers(modifiers)?,
        button: match button {
            "btn_left" => BTN_LEFT,
            "btn_right" => BTN_RIGHT,
            "btn_middle" => BTN_MIDDLE,
            _ => return Err(format!("unknown button `{button}`")),
        },
        action: match action {
            "move" => MouseAction::Move,
            "resize" => MouseAction::Resize,
            _ => return Err(format!("unknown mouse action `{action}`")),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUPER: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: false,
        logo: true,
    };

    fn parse_clean(text: &str) -> Config {
        let (config, errors) = Config::parse(text);
        assert_eq!(errors, []);
        config
    }

    fn action_of(config: &Config, modifiers: Modifiers, key: u32) -> Option<&Action> {
        config
            .binds
            .iter()
            .find(|bind| bind.modifiers == modifiers && bind.key.raw() == key)
            .map(|bind| &bind.action)
    }

    #[test]
    fn defaults_match_the_mango_setup() {
        let config = Config::default();
        assert_eq!(config.keyboard_layout, "de");
        assert_eq!(
            config.gaps,
            Gaps {
                outer: 10,
                inner: 5
            }
        );
        assert_eq!(config.border_width, 4);
        assert_eq!((config.repeat_delay, config.repeat_rate), (600, 25));
        assert!(config.sloppy_focus);
        assert_eq!(
            action_of(&config, SUPER, keysyms::KEY_Return),
            Some(&Action::Spawn("kitty".into()))
        );
        assert_eq!(
            action_of(&config, SUPER, keysyms::KEY_m),
            Some(&Action::Quit)
        );
        assert_eq!(config.mouse_binds.len(), 2);
    }

    #[test]
    fn an_empty_file_means_all_defaults() {
        assert_eq!(parse_clean("\n# nothing\n  \n"), Config::default());
    }

    #[test]
    fn options_override_single_defaults() {
        let config = parse_clean(
            "gap_outer = 3\n\
             border_color = 0xff0000\n\
             focus_color = #00ff0080\n\
             keyboard_layout = us\n\
             keyboard_variant = dvorak\n\
             sloppy_focus = 0\n",
        );
        assert_eq!(config.gaps.outer, 3);
        assert_eq!(config.gaps.inner, 5);
        assert_eq!(config.border_color, [1.0, 0.0, 0.0, 1.0]);
        let alpha = 128.0 / 255.0;
        assert_eq!(config.focus_color, [0.0, alpha, 0.0, alpha]);
        assert_eq!(config.keyboard_layout, "us");
        assert_eq!(config.xkb().variant, "dvorak");
        assert!(!config.sloppy_focus);
        assert_eq!(config.binds, Config::default().binds);
    }

    #[test]
    fn own_binds_replace_the_defaults_but_never_the_way_out() {
        let config = parse_clean("bind = ALT, t, spawn, foot\n");
        assert_eq!(config.binds.len(), 2);
        assert_eq!(config.binds[0].action, Action::Quit);
        assert_eq!(config.binds[1].action, Action::Spawn("foot".into()));
        assert_eq!(config.mouse_binds, Config::default().mouse_binds);

        // Even a file that takes the key away keeps it working: the first match wins.
        let config = parse_clean("bind = SUPER, m, close\n");
        assert_eq!(config.binds[0].action, Action::Quit);

        let config = parse_clean("bind = ALT, x, quit\n");
        assert_eq!(config.binds.len(), 1);
    }

    #[test]
    fn a_broken_bind_still_counts_as_replacing_the_defaults() {
        let (config, errors) = Config::parse("bind = HYPER, x, close\n");
        assert_eq!(errors.len(), 1);
        assert_eq!(config.binds.len(), 1);
        assert_eq!(config.binds[0].action, Action::Quit);
    }

    #[test]
    fn own_mouse_binds_replace_the_defaults() {
        let config = parse_clean("mousebind = ALT, btn_middle, resize\n");
        assert_eq!(
            config.mouse_binds,
            [MouseBind {
                modifiers: Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
                button: BTN_MIDDLE,
                action: MouseAction::Resize,
            }]
        );
        assert_eq!(
            config.action_for_button(config.mouse_binds[0].modifiers, BTN_MIDDLE),
            Some(MouseAction::Resize)
        );
        assert_eq!(config.action_for_button(SUPER, BTN_MIDDLE), None);
    }

    #[test]
    fn binds_parse_every_action() {
        let cases = [
            ("close", Action::Close),
            ("quit", Action::Quit),
            ("reload", Action::Reload),
            ("focus, left", Action::Focus(Direction::Left)),
            ("focus_next", Action::FocusNext),
            ("focus_prev", Action::FocusPrevious),
            ("swap, down", Action::Swap(Direction::Down)),
            ("split_ratio, +0.05", Action::SplitRatio(0.05)),
            ("split_ratio, -0.1", Action::SplitRatio(-0.1)),
            ("float", Action::Float),
            ("fullscreen", Action::Fullscreen),
            ("maximize", Action::Maximize),
            ("workspace, 9", Action::Workspace(WorkspaceTarget::Index(8))),
            ("workspace, prev", Action::Workspace(WorkspaceTarget::Previous)),
            ("workspace, next", Action::Workspace(WorkspaceTarget::Next)),
            ("move_to_workspace, 1", Action::MoveToWorkspace(0)),
            ("focus_output, up", Action::FocusOutput(Direction::Up)),
            ("move_to_output, right", Action::MoveToOutput(Direction::Right)),
        ];
        for (text, action) in cases {
            let bind = parse_bind(&format!("SUPER, a, {text}")).unwrap();
            assert_eq!(bind.action, action, "{text}");
        }
    }

    #[test]
    fn spawn_keeps_commas_and_equals_signs() {
        let bind = parse_bind("NONE, F1, spawn, sh -c 'a=1, b=2'").unwrap();
        assert_eq!(bind.action, Action::Spawn("sh -c 'a=1, b=2'".into()));
        assert_eq!(bind.modifiers, Modifiers::default());
        let config = parse_clean("bind = NONE, F1, spawn, env A=1 foo, bar\n");
        assert_eq!(config.binds[1].action, Action::Spawn("env A=1 foo, bar".into()));
    }

    #[test]
    fn modifiers_combine_in_any_case() {
        let bind = parse_bind("super+Shift + CTRL+alt, x, close").unwrap();
        assert_eq!(
            bind.modifiers,
            Modifiers {
                ctrl: true,
                alt: true,
                shift: true,
                logo: true
            }
        );
    }

    #[test]
    fn single_letters_are_stored_in_lower_case() {
        assert_eq!(parse_key("I").unwrap().raw(), keysyms::KEY_i);
        assert_eq!(parse_key("ß").unwrap().raw(), keysyms::KEY_ssharp);
        assert_eq!(parse_key("Ä").unwrap().raw(), keysyms::KEY_adiaeresis);
        assert_eq!(parse_key("ssharp").unwrap().raw(), keysyms::KEY_ssharp);
        assert_eq!(parse_key("BackSpace").unwrap().raw(), keysyms::KEY_BackSpace);
        assert_eq!(parse_key("backspace").unwrap().raw(), keysyms::KEY_BackSpace);
        assert_eq!(parse_key("1").unwrap().raw(), keysyms::KEY_1);
    }

    #[test]
    fn errors_name_the_line_and_good_lines_survive() {
        let (config, errors) = Config::parse(
            "gap_outer = 7\n\
             # comment\n\
             gap_inner = lots\n\
             no equals sign\n\
             wobble = 1\n\
             bind = SUPER, nosuchkey, close\n\
             bind = SUPER, a, explode\n\
             bind = SUPER, a\n\
             bind = SUPER, a, close, now\n\
             bind = SUPER, a, focus, sideways\n\
             bind = SUPER, a, workspace, 10\n\
             bind = SUPER, a, spawn\n\
             bind = SUPER, a, split_ratio, NaN\n\
             mousebind = SUPER, btn_side, move\n\
             mousebind = SUPER, btn_left, poke\n\
             mousebind = SUPER\n\
             border_color = red\n\
             border_color = 0x12345\n\
             sloppy_focus = maybe\n",
        );
        assert_eq!(config.gaps.outer, 7);
        let lines: Vec<usize> = errors.iter().map(|error| error.line).collect();
        assert_eq!(
            lines,
            [3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19]
        );
        assert_eq!(errors[2].to_string(), "line 5: unknown option `wobble`");
    }

    #[test]
    fn load_treats_a_missing_file_as_defaults() {
        let (config, errors) = Config::load(Path::new("/nonexistent/wirbel/config")).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(errors, []);
    }

    #[test]
    fn load_reports_an_unreadable_path() {
        assert!(Config::load(Path::new("/")).is_err());
    }
}
