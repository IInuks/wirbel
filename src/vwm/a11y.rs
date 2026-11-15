//! The accessibility interface (AT-SPI) of the programs in an invisible window: their controls
//! and texts as a tree, pressing and filling controls without the pointer, and whether a
//! document is still loading. Toolkits such as GTK and Qt and the browsers offer it; games and
//! most terminals do not.
//!
//! A shadow has a session bus of its own, and with it an accessibility bus of its own, so what
//! is found here belongs to that window alone. With `open --share-bus` the programs are on the
//! bus of the desktop, among everything else that runs there.
//!
//! Built only with the feature `vwm-a11y`; without it every call says so.

use super::wait::glob;

/// One control or text in a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// How deep in the tree, the program itself is 0.
    pub depth: usize,
    /// What it is: `push button`, `entry`, `link`, `document web`, ...
    pub role: String,
    pub name: String,
    /// The text in it, for what holds text.
    pub text: String,
    /// Whether it is still loading or working.
    pub busy: bool,
    pub actions: bool,
    pub editable: bool,
    /// Where it lives: bus name and object path.
    pub address: (String, String),
}

impl Item {
    /// As a line of `wirbel vwm ui NAME tree`.
    pub fn line(&self) -> String {
        let mut line = format!("{}{}", "  ".repeat(self.depth), self.role);
        if !self.name.is_empty() {
            line += &format!(" \"{}\"", self.name);
        }
        let text = self.text.replace('\n', "\\n");
        if !text.is_empty() && text != self.name {
            let short: String = text.chars().take(120).collect();
            line += &format!(" [{short}]");
        }
        if self.busy {
            line += " (busy)";
        }
        line
    }
}

/// The lines of text in a tree: the texts, and the names of what has no text of its own,
/// without saying the same thing twice in a row.
pub fn text_of(items: &[Item]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for item in items {
        // The object replacement character stands for an embedded child, which comes anyway.
        let own = item.text.replace('\u{fffc}', "");
        let said = if own.trim().is_empty() {
            &item.name
        } else {
            &own
        };
        for line in said.lines().map(str::trim).filter(|line| !line.is_empty()) {
            if lines.last().map(String::as_str) != Some(line) {
                lines.push(line.to_string());
            }
        }
    }
    lines
}

/// The first item whose name or text matches, among those that fit `wanted`.
pub fn find<'a>(
    items: &'a [Item],
    pattern: &str,
    wanted: impl Fn(&Item) -> bool,
) -> Option<&'a Item> {
    let fits = |item: &&Item| glob(pattern, &item.name) || glob(pattern, &item.text);
    // An exact name wins over one that only holds the pattern.
    let exact = items
        .iter()
        .find(|item| wanted(item) && item.name == pattern);
    exact.or_else(|| items.iter().filter(|item| wanted(item)).find(fits))
}

/// Whether the documents in a tree have loaded. `None` if there is no document.
pub fn loaded(items: &[Item]) -> Option<bool> {
    let mut documents = items
        .iter()
        .filter(|item| item.role.starts_with("document"))
        .peekable();
    documents.peek()?;
    Some(documents.all(|document| !document.busy))
}

#[cfg(not(feature = "vwm-a11y"))]
mod bus {
    use super::Item;

    const MISSING: &str =
        "this wirbel was built without the accessibility interface (feature vwm-a11y)";

    pub struct Tree;

    impl Tree {
        pub fn of(_display: &str) -> Result<Self, String> {
            Err(MISSING.into())
        }
        pub fn items(&self) -> Result<Vec<Item>, String> {
            Err(MISSING.into())
        }
        pub fn press(&self, _item: &Item) -> Result<(), String> {
            Err(MISSING.into())
        }
        pub fn set(&self, _item: &Item, _text: &str) -> Result<(), String> {
            Err(MISSING.into())
        }
    }
}

#[cfg(feature = "vwm-a11y")]
mod bus {
    use zbus::{
        blocking::{Connection, connection::Builder},
        zvariant::{OwnedObjectPath, OwnedValue, Value},
    };

    use super::Item;
    use crate::vwm::cli::ask_display;

    const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
    const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
    /// The number of the state "busy" in the bit set of `GetState`.
    const STATE_BUSY: u32 = 3;
    /// Larger trees than this are cut: a browser can have tens of thousands of nodes.
    const MAX_ITEMS: usize = 4000;
    const MAX_DEPTH: usize = 40;

    fn fail(what: &str, err: impl std::fmt::Display) -> String {
        format!("accessibility: {what}: {err}")
    }

    /// The accessibility bus of an invisible window.
    pub struct Tree {
        bus: Connection,
    }

    type Address = (String, OwnedObjectPath);

    impl Tree {
        /// Connects to the accessibility bus of the shadow on this Wayland display, and turns
        /// accessibility on there: programs build their tree only when someone asks.
        pub fn of(display: &str) -> Result<Self, String> {
            let answer = ask_display(display, "shadow bus")?;
            let address = answer
                .strip_prefix("ok ")
                .ok_or_else(|| answer.strip_prefix("error ").unwrap_or(&answer).to_string())?;
            let session = Builder::address(address)
                .and_then(Builder::build)
                .map_err(|err| fail("no session bus of the window", err))?;
            let on = Value::Bool(true);
            for property in ["IsEnabled", "ScreenReaderEnabled"] {
                // Not every version has both, and the tree is there without them for some.
                let _ = session.call_method(
                    Some("org.a11y.Bus"),
                    "/org/a11y/bus",
                    Some(PROPERTIES),
                    "Set",
                    &("org.a11y.Status", property, &on),
                );
            }
            let reply = session
                .call_method(
                    Some("org.a11y.Bus"),
                    "/org/a11y/bus",
                    Some("org.a11y.Bus"),
                    "GetAddress",
                    &(),
                )
                .map_err(|err| fail("no accessibility bus (is at-spi2-core installed?)", err))?;
            let address: String = reply
                .body()
                .deserialize()
                .map_err(|err| fail("bad address", err))?;
            let bus = Builder::address(address.as_str())
                .and_then(Builder::build)
                .map_err(|err| fail("cannot connect", err))?;
            Ok(Self { bus })
        }

        fn call<B>(
            &self,
            at: &Address,
            interface: &str,
            method: &str,
            body: &B,
        ) -> zbus::Result<zbus::Message>
        where
            B: zbus::export::serde::Serialize + zbus::zvariant::DynamicType,
        {
            self.bus.call_method(
                Some(at.0.as_str()),
                at.1.as_str(),
                Some(interface),
                method,
                body,
            )
        }

        fn text_property(&self, at: &Address, name: &str) -> String {
            let reply = self.call(at, PROPERTIES, "Get", &(ACCESSIBLE, name));
            let value = reply
                .ok()
                .and_then(|reply| reply.body().deserialize::<OwnedValue>().ok());
            value
                .and_then(|value| String::try_from(value).ok())
                .unwrap_or_default()
        }

        fn item(&self, at: &Address, depth: usize) -> Item {
            let get = |method: &str| self.call(at, ACCESSIBLE, method, &()).ok();
            let role =
                get("GetRoleName").and_then(|reply| reply.body().deserialize::<String>().ok());
            let states =
                get("GetState").and_then(|reply| reply.body().deserialize::<Vec<u32>>().ok());
            let interfaces = get("GetInterfaces")
                .and_then(|reply| reply.body().deserialize::<Vec<String>>().ok());
            let interfaces = interfaces.unwrap_or_default();
            let has = |name: &str| interfaces.iter().any(|interface| interface.ends_with(name));
            let text = match has(".Text") {
                true => {
                    let reply = self.call(at, "org.a11y.atspi.Text", "GetText", &(0i32, -1i32));
                    reply
                        .ok()
                        .and_then(|reply| reply.body().deserialize::<String>().ok())
                }
                false => None,
            };
            let busy = states.is_some_and(|states| {
                let word = states.get((STATE_BUSY / 32) as usize).copied().unwrap_or(0);
                word & (1 << (STATE_BUSY % 32)) != 0
            });
            Item {
                depth,
                role: role.unwrap_or_default(),
                name: self.text_property(at, "Name"),
                text: text.unwrap_or_default(),
                busy,
                actions: has(".Action"),
                editable: has(".EditableText"),
                address: (at.0.clone(), at.1.to_string()),
            }
        }

        fn children(&self, at: &Address) -> Vec<Address> {
            let reply = self.call(at, ACCESSIBLE, "GetChildren", &());
            let children = reply
                .ok()
                .and_then(|reply| reply.body().deserialize::<Vec<Address>>().ok());
            children.unwrap_or_default()
        }

        /// Everything the programs on this bus offer, parents before their children.
        pub fn items(&self) -> Result<Vec<Item>, String> {
            let root: Address = (
                "org.a11y.atspi.Registry".to_string(),
                OwnedObjectPath::try_from("/org/a11y/atspi/accessible/root")
                    .map_err(|err| fail("path", err))?,
            );
            // Asking the registry fails if nothing answers on the bus at all.
            self.call(&root, ACCESSIBLE, "GetChildren", &())
                .map_err(|err| fail("the registry does not answer", err))?;
            let mut items = Vec::new();
            let mut stack: Vec<(Address, usize)> = self
                .children(&root)
                .into_iter()
                .rev()
                .map(|app| (app, 0))
                .collect();
            while let Some((at, depth)) = stack.pop() {
                if items.len() >= MAX_ITEMS {
                    break;
                }
                items.push(self.item(&at, depth));
                if depth < MAX_DEPTH {
                    stack.extend(
                        self.children(&at)
                            .into_iter()
                            .rev()
                            .map(|child| (child, depth + 1)),
                    );
                }
            }
            Ok(items)
        }

        fn address(item: &Item) -> Result<Address, String> {
            let path = OwnedObjectPath::try_from(item.address.1.as_str())
                .map_err(|err| fail("path", err))?;
            Ok((item.address.0.clone(), path))
        }

        /// Does what a click on the control does.
        pub fn press(&self, item: &Item) -> Result<(), String> {
            let at = Self::address(item)?;
            let reply = self
                .call(&at, "org.a11y.atspi.Action", "DoAction", &(0i32,))
                .map_err(|err| fail("the control cannot be pressed", err))?;
            match reply.body().deserialize::<bool>() {
                Ok(false) => Err("accessibility: the control refused".into()),
                _ => Ok(()),
            }
        }

        /// Replaces the text of a field.
        pub fn set(&self, item: &Item, text: &str) -> Result<(), String> {
            let at = Self::address(item)?;
            let reply = self
                .call(
                    &at,
                    "org.a11y.atspi.EditableText",
                    "SetTextContents",
                    &(text,),
                )
                .map_err(|err| fail("the field cannot be filled", err))?;
            match reply.body().deserialize::<bool>() {
                Ok(false) => Err("accessibility: the field refused the text".into()),
                _ => Ok(()),
            }
        }
    }
}

pub use bus::Tree;

#[cfg(test)]
mod tests {
    use super::*;

    fn item(depth: usize, role: &str, name: &str, text: &str) -> Item {
        Item {
            depth,
            role: role.into(),
            name: name.into(),
            text: text.into(),
            busy: false,
            actions: role == "push button",
            editable: role == "entry",
            address: (":1.5".into(), "/a".into()),
        }
    }

    fn page() -> Vec<Item> {
        vec![
            item(0, "application", "Firefox", ""),
            item(1, "frame", "Example Domain", ""),
            item(2, "entry", "Search or enter address", "example.com"),
            item(2, "document web", "Example Domain", ""),
            item(3, "heading", "Example Domain", "Example Domain"),
            item(
                3,
                "paragraph",
                "",
                "This domain is for use in examples.\nMore \u{fffc}",
            ),
            item(4, "link", "More information...", "More information..."),
            item(2, "push button", "Reload", ""),
            item(2, "push button", "Reload all", ""),
        ]
    }

    #[test]
    fn a_tree_reads_as_lines_and_as_text() {
        let items = page();
        assert_eq!(items[0].line(), "application \"Firefox\"");
        assert_eq!(
            items[2].line(),
            "    entry \"Search or enter address\" [example.com]"
        );
        assert_eq!(items[4].line(), "      heading \"Example Domain\"");
        assert_eq!(
            items[5].line(),
            "      paragraph [This domain is for use in examples.\\nMore \u{fffc}]"
        );
        assert_eq!(
            text_of(&items),
            [
                "Firefox",
                "Example Domain",
                "example.com",
                "Example Domain",
                "This domain is for use in examples.",
                "More",
                "More information...",
                "Reload",
                "Reload all",
            ]
        );
    }

    #[test]
    fn controls_are_found_by_name_or_text() {
        let items = page();
        let button = |item: &Item| item.actions;
        assert_eq!(find(&items, "Reload", button), Some(&items[7]));
        assert_eq!(find(&items, "Reload a*", button), Some(&items[8]));
        assert_eq!(find(&items, "Example", button), None);
        assert_eq!(
            find(&items, "example.com", |item| item.editable),
            Some(&items[2])
        );
        assert_eq!(find(&items, "*address", |_| true), Some(&items[2]));
    }

    #[test]
    fn documents_load() {
        let mut items = page();
        assert_eq!(loaded(&items), Some(true));
        items[3].busy = true;
        assert_eq!(loaded(&items), Some(false));
        assert!(items[3].line().ends_with("(busy)"));
        assert_eq!(loaded(&items[..3]), None);
    }
}
