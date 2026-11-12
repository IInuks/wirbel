//! Extensions: code built into a wirbel binary that the core does not know by name.
//!
//! A program that embeds wirbel passes its extensions to [`crate::app::main`] (the `wirbel`
//! binary passes none). Each one has a name, which works as a word in three places:
//!
//! - a bind action, `bind = SUPER, F12, NAME, ARGUMENT`,
//! - a socket request, `NAME ARGUMENT`, answered with `ok`, another answer, or `error MESSAGE`,
//! - the command line, `wirbel NAME ARGUMENT`, which sends that request.
//!
//! All three end in [`Extension::command`]. Besides that an extension hears of the start
//! ([`Extension::init`]) and of every configuration reload ([`Extension::reload`]). To change
//! what the compositor does it uses the generic hooks of the core, set through [`Wirbel`]: the
//! render override ([`crate::scene_override`]), the input filter ([`crate::input_filter`]) and
//! the window hook ([`crate::window_hook`]).
//!
//! Its state lives in [`Extensions`], a slot per type: `state.extensions.insert(MyState::default())`,
//! later `state.extensions.get_mut::<MyState>()`. That borrows only the `extensions` field, so the
//! rest of the state stays usable at the same time.
//!
//! The names and argument checks are also kept for the whole process ([`register`]), because the
//! configuration is parsed before there is a compositor, and again on every reload.

use std::{
    any::{Any, TypeId},
    collections::HashMap,
    rc::Rc,
    sync::Mutex,
};

use crate::Wirbel;

/// What the configuration parser, the socket and the command line know of an extension.
#[derive(Debug, Clone, Copy)]
pub struct ExtensionInfo {
    /// The word of its bind action, socket request and command. Lower case, no spaces, not the
    /// name of a core action or request.
    pub name: &'static str,
    /// Says whether an argument of the bind action is good, with the reason if not. A config line
    /// with a bad one is skipped like any other bad line.
    pub check: fn(&str) -> Result<(), String>,
    /// Its line in the usage of `--help`, such as `wirbel NAME on|off`.
    pub usage: &'static str,
    /// A paragraph for `--help`, may be empty.
    pub help: &'static str,
}

/// Code built into the compositor, see the module documentation.
pub trait Extension {
    fn info(&self) -> ExtensionInfo;

    /// Once at startup, after the configuration and its path are known and before the backend
    /// makes the outputs. Work that needs them belongs in an idle (`state.loop_handle`).
    fn init(&self, _state: &mut Wirbel) {}

    /// After the configuration was reloaded and replaced `state.config`.
    fn reload(&self, _state: &mut Wirbel) {}

    /// A bind action, socket request or command with this extension's name. `Ok` is the answer
    /// for the socket (`ok` when there is nothing to say), `Err` the message of its error.
    fn command(&self, state: &mut Wirbel, argument: &str) -> Result<String, String>;
}

/// The extensions of a compositor and their state.
#[derive(Default)]
pub struct Extensions {
    list: Vec<Rc<dyn Extension>>,
    slots: HashMap<TypeId, Box<dyn Any>>,
}

impl Extensions {
    /// Stores a value as the state of type `T`, returning the one it replaces.
    pub fn insert<T: Any>(&mut self, value: T) -> Option<T> {
        self.slots
            .insert(TypeId::of::<T>(), Box::new(value))
            .and_then(|old| old.downcast().ok())
            .map(|old| *old)
    }

    pub fn get<T: Any>(&self) -> Option<&T> {
        self.slots.get(&TypeId::of::<T>())?.downcast_ref()
    }

    pub fn get_mut<T: Any>(&mut self) -> Option<&mut T> {
        self.slots.get_mut(&TypeId::of::<T>())?.downcast_mut()
    }

    /// The extension with this name.
    pub fn find(&self, name: &str) -> Option<Rc<dyn Extension>> {
        self.list
            .iter()
            .find(|extension| extension.info().name == name)
            .cloned()
    }

    /// All of them, in the order they were added. A copy, so that they can be called with the
    /// whole state.
    pub fn all(&self) -> Vec<Rc<dyn Extension>> {
        self.list.clone()
    }
}

impl Wirbel {
    /// Adds an extension and makes its name known (see [`register`]). It does not run `init`.
    /// One with the same name as an earlier one replaces it.
    pub fn add_extension(&mut self, extension: Rc<dyn Extension>) {
        let info = extension.info();
        register(info);
        self.extensions
            .list
            .retain(|other| other.info().name != info.name);
        self.extensions.list.push(extension);
    }

    /// Runs the command of the extension with this name, see [`Extension::command`].
    pub fn extension_command(&mut self, name: &str, argument: &str) -> Result<String, String> {
        let extension = self
            .extensions
            .find(name)
            .ok_or_else(|| format!("no extension `{name}` in this wirbel"))?;
        extension.command(self, argument)
    }

    /// Part of the startup: every extension's `init`.
    pub fn init_extensions(&mut self) {
        for extension in self.extensions.all() {
            extension.init(self);
        }
    }

    /// Part of a reload: every extension's `reload`.
    pub(crate) fn reload_extensions(&mut self) {
        for extension in self.extensions.all() {
            extension.reload(self);
        }
    }
}

static REGISTERED: Mutex<Vec<ExtensionInfo>> = Mutex::new(Vec::new());

/// Makes the name of an extension a word of the configuration, the socket and the command line
/// of this process. [`Wirbel::add_extension`] does it too; whoever parses a configuration before
/// there is a compositor calls it first. Registering a name again replaces it.
pub fn register(info: ExtensionInfo) {
    let mut registered = REGISTERED.lock().unwrap_or_else(|err| err.into_inner());
    registered.retain(|other| other.name != info.name);
    registered.push(info);
}

/// The registered extension with this name.
pub fn registered(name: &str) -> Option<ExtensionInfo> {
    let registered = REGISTERED.lock().unwrap_or_else(|err| err.into_inner());
    registered.iter().find(|info| info.name == name).copied()
}

/// A socket request for a registered extension: its name and the rest of the line as the
/// argument.
pub(crate) fn parse_request(line: &str) -> Option<(&'static str, String)> {
    let mut words = line.split_ascii_whitespace();
    let info = registered(words.next()?)?;
    Some((info.name, words.collect::<Vec<_>>().join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn even(argument: &str) -> Result<(), String> {
        match argument.parse::<u32>() {
            Ok(number) if number % 2 == 0 => Ok(()),
            _ => Err(format!("expected an even number, got `{argument}`")),
        }
    }

    const INFO: ExtensionInfo = ExtensionInfo {
        name: "test-even",
        check: even,
        usage: "wirbel test-even N",
        help: "",
    };

    #[test]
    fn slots_are_by_type() {
        let mut extensions = Extensions::default();
        assert_eq!(extensions.get::<u32>(), None);
        assert_eq!(extensions.insert(5_u32), None);
        assert_eq!(extensions.insert("five"), None);
        *extensions.get_mut::<u32>().unwrap() += 1;
        assert_eq!(extensions.get::<u32>(), Some(&6));
        assert_eq!(extensions.get::<&str>(), Some(&"five"));
        assert_eq!(extensions.insert(7_u32), Some(6));
    }

    #[test]
    fn registered_names_are_requests_and_actions() {
        register(INFO);
        assert_eq!(
            parse_request("test-even  4 \n"),
            Some(("test-even", "4".to_string()))
        );
        assert_eq!(parse_request("test-odd 4"), None);
        assert_eq!(parse_request(""), None);
        let (config, errors) = crate::config::Config::parse(
            "bind = SUPER, a, test-even, 4\nbind = SUPER, b, test-even, 3\n",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(config.binds.iter().any(|bind| bind.action
            == crate::config::Action::Extension {
                name: "test-even".into(),
                argument: "4".into()
            }));
    }
}
