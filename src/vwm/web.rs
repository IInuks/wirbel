//! The remote control of browsers: running JavaScript in the page of an invisible browser,
//! which gives the exact text, clicks by CSS selector and a real "loaded".
//!
//! A browser started with `wirbel vwm open --web` listens on a port of this machine and writes
//! the port into its profile folder: Firefox speaks WebDriver BiDi, the Chromium family the
//! Chrome DevTools Protocol. Both are JSON over a WebSocket; the little of WebSocket that takes
//! is done here. Whoever can reach that port controls that browser, which is why it is asked
//! for and not the rule; the browser only listens on this machine.

use std::{
    io::{Read, Write},
    net::TcpStream,
    path::Path,
    time::Duration,
};

use super::json::{Value, quote};

const PATIENCE: Duration = Duration::from_secs(30);

/// A WebSocket connection as a client, for text messages.
pub struct Socket {
    stream: TcpStream,
    /// For the masks every frame of a client has to carry. They hide nothing here.
    mask: u32,
}

fn io(err: std::io::Error) -> String {
    format!("the browser does not answer: {err}")
}

impl Socket {
    /// Connects to `ws://HOST:PORT/PATH`.
    pub fn connect(address: &str, path: &str) -> Result<Self, String> {
        let mut stream = TcpStream::connect(address).map_err(io)?;
        stream.set_read_timeout(Some(PATIENCE)).map_err(io)?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: d2lyYmVsLXZ3bS13ZWItMQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).map_err(io)?;
        // The answer ends with an empty line; nothing of ours follows it unasked.
        let mut head = Vec::new();
        let mut byte = [0];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).map_err(io)?;
            head.push(byte[0]);
            if head.len() > 16 * 1024 {
                return Err("the browser sent no WebSocket answer".into());
            }
        }
        let head = String::from_utf8_lossy(&head);
        if !head.starts_with("HTTP/1.1 101") {
            let first = head.lines().next().unwrap_or_default();
            return Err(format!("the browser refused the connection: {first}"));
        }
        let mask = std::process::id().wrapping_mul(2654435761);
        Ok(Self { stream, mask })
    }

    fn frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut frame = vec![0x80 | opcode];
        match payload.len() {
            length @ 0..126 => frame.push(0x80 | length as u8),
            length @ 126..65536 => {
                frame.push(0x80 | 126);
                frame.extend((length as u16).to_be_bytes());
            }
            length => {
                frame.push(0x80 | 127);
                frame.extend((length as u64).to_be_bytes());
            }
        }
        self.mask = self.mask.wrapping_mul(1664525).wrapping_add(1013904223);
        let mask = self.mask.to_be_bytes();
        frame.extend(mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, byte)| byte ^ mask[i % 4]),
        );
        self.stream.write_all(&frame).map_err(io)
    }

    pub fn send(&mut self, text: &str) -> Result<(), String> {
        self.frame(1, text.as_bytes())
    }

    /// The next text message, put together from its pieces. Pings are answered on the way.
    pub fn receive(&mut self) -> Result<String, String> {
        let mut message = Vec::new();
        loop {
            let mut head = [0; 2];
            self.stream.read_exact(&mut head).map_err(io)?;
            let (last, opcode) = (head[0] & 0x80 != 0, head[0] & 0x0f);
            let length = match head[1] & 0x7f {
                126 => {
                    let mut bytes = [0; 2];
                    self.stream.read_exact(&mut bytes).map_err(io)?;
                    u64::from(u16::from_be_bytes(bytes))
                }
                127 => {
                    let mut bytes = [0; 8];
                    self.stream.read_exact(&mut bytes).map_err(io)?;
                    u64::from_be_bytes(bytes)
                }
                short => u64::from(short),
            };
            if length > 256 * 1024 * 1024 {
                return Err("the browser sent more than 256 MB at once".into());
            }
            let mut mask = [0; 4];
            if head[1] & 0x80 != 0 {
                self.stream.read_exact(&mut mask).map_err(io)?;
            }
            let mut payload = vec![0; length as usize];
            self.stream.read_exact(&mut payload).map_err(io)?;
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[i % 4];
            }
            match opcode {
                8 => return Err("the browser closed the connection".into()),
                9 => self.frame(10, &payload)?,
                10 => {}
                _ => {
                    message.extend(payload);
                    if last {
                        return Ok(String::from_utf8_lossy(&message).into_owned());
                    }
                }
            }
        }
    }
}

/// The body of `GET http://ADDRESS/PATH`.
fn http_get(address: &str, path: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(address).map_err(io)?;
    stream.set_read_timeout(Some(PATIENCE)).map_err(io)?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).map_err(io)?;
    // Not every browser closes the connection when asked: read as much as the head announces.
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).map_err(io)?;
        head.push(byte[0]);
        if head.len() > 64 * 1024 {
            return Err("the browser sent no answer".into());
        }
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"));
    let length = length.and_then(|length| length.trim().parse::<usize>().ok());
    let mut body = Vec::new();
    match length {
        Some(length) => {
            body.resize(length.min(64 * 1024 * 1024), 0);
            stream.read_exact(&mut body).map_err(io)?;
        }
        None => {
            stream.read_to_end(&mut body).map_err(io)?;
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Which protocol a browser speaks, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// Firefox: WebDriver BiDi at this address.
    Bidi(String),
    /// Chromium: the DevTools protocol at this address.
    Cdp(String),
}

/// What the browser wrote into its profile folder about its port.
pub fn endpoint(profile: &Path) -> Result<Endpoint, String> {
    if let Ok(text) = std::fs::read_to_string(profile.join("WebDriverBiDiServer.json")) {
        let value = Value::parse(&text)?;
        let host = value
            .get("ws_host")
            .and_then(Value::as_str)
            .unwrap_or("127.0.0.1");
        let port = value
            .get("ws_port")
            .and_then(Value::as_f64)
            .ok_or("no port in WebDriverBiDiServer.json")?;
        return Ok(Endpoint::Bidi(format!("{host}:{port}")));
    }
    if let Ok(text) = std::fs::read_to_string(profile.join("DevToolsActivePort")) {
        let port = text
            .lines()
            .next()
            .and_then(|line| line.trim().parse::<u16>().ok());
        return Ok(Endpoint::Cdp(format!(
            "127.0.0.1:{}",
            port.ok_or("no port in DevToolsActivePort")?
        )));
    }
    Err(
        "this window is no browser with remote control: start it with `wirbel vwm open --web`"
            .into(),
    )
}

/// The options that make a browser listen, by the name of its program. `None` for programs
/// that are no browser we know.
pub fn listen_options(program: &str) -> Option<&'static str> {
    let firefox = [
        "firefox",
        "firefox-esr",
        "librewolf",
        "waterfox",
        "zen-browser",
    ];
    let chromium = [
        "chromium",
        "chrome",
        "google-chrome",
        "brave",
        "vivaldi",
        "microsoft-edge",
        "opera",
    ];
    if firefox.contains(&program) {
        Some("--remote-debugging-port 0")
    } else if chromium.iter().any(|name| program.starts_with(name)) {
        Some("--remote-debugging-port=0")
    } else {
        None
    }
}

/// A page of a browser to run JavaScript in.
pub struct Page {
    socket: Socket,
    next: u64,
    /// BiDi: the tab. CDP has one connection per tab.
    context: Option<String>,
}

impl Page {
    /// The first tab of the browser at this endpoint.
    pub fn open(endpoint: &Endpoint) -> Result<Self, String> {
        match endpoint {
            Endpoint::Cdp(address) => {
                let targets = Value::parse(&http_get(address, "/json")?)?;
                let page = targets
                    .as_array()
                    .iter()
                    .find(|target| target.get("type").and_then(Value::as_str) == Some("page"));
                let url = page
                    .and_then(|page| page.get("webSocketDebuggerUrl"))
                    .and_then(Value::as_str);
                let url = url.ok_or("the browser has no page open")?;
                let path = url
                    .strip_prefix("ws://")
                    .and_then(|rest| rest.find('/').map(|at| &rest[at..]));
                let socket =
                    Socket::connect(address, path.ok_or("unexpected address of the page")?)?;
                Ok(Self {
                    socket,
                    next: 0,
                    context: None,
                })
            }
            Endpoint::Bidi(address) => {
                let socket = Socket::connect(address, "/session")?;
                let mut page = Self {
                    socket,
                    next: 0,
                    context: None,
                };
                page.call("session.new", "{\"capabilities\":{}}")?;
                let tree = page.call("browsingContext.getTree", "{}")?;
                let first = tree
                    .get("contexts")
                    .map(Value::as_array)
                    .and_then(<[Value]>::first);
                let context = first
                    .and_then(|context| context.get("context"))
                    .and_then(Value::as_str);
                page.context = Some(context.ok_or("the browser has no page open")?.to_string());
                Ok(page)
            }
        }
    }

    /// Sends a command and waits for its answer, past whatever events come meanwhile.
    fn call(&mut self, method: &str, params: &str) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        self.socket.send(&format!(
            "{{\"id\":{id},\"method\":{},\"params\":{params}}}",
            quote(method)
        ))?;
        loop {
            let message = Value::parse(&self.socket.receive()?)?;
            if message.get("id").and_then(Value::as_f64) != Some(id as f64) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .or(message.get("message"))
                    .and_then(Value::as_str);
                let text = text.map(str::to_string).unwrap_or_else(|| error.to_json());
                return Err(format!("the browser says: {text}"));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Runs JavaScript in the page and returns what it evaluates to. A promise is waited for.
    pub fn eval(&mut self, script: &str) -> Result<Value, String> {
        match self.context.clone() {
            None => {
                let params = format!(
                    "{{\"expression\":{},\"returnByValue\":true,\"awaitPromise\":true}}",
                    quote(script)
                );
                let result = self.call("Runtime.evaluate", &params)?;
                if let Some(details) = result.get("exceptionDetails") {
                    let text = details
                        .at(&["exception", "description"])
                        .or(details.get("text"));
                    let text = text.and_then(Value::as_str).unwrap_or("the script failed");
                    // Without the lines that say where in our own wrapper it happened.
                    let text = text.lines().next().unwrap_or(text);
                    return Err(format!("the page says: {text}"));
                }
                Ok(result
                    .at(&["result", "value"])
                    .cloned()
                    .unwrap_or(Value::Null))
            }
            Some(context) => {
                // BiDi hands back its own kind of values; JSON text is the same everywhere.
                let wrapped = format!(
                    "Promise.resolve({script}\n).then(v => JSON.stringify(v === undefined ? null : v))"
                );
                let params = format!(
                    "{{\"expression\":{},\"target\":{{\"context\":{}}},\"awaitPromise\":true}}",
                    quote(&wrapped),
                    quote(&context)
                );
                let result = self.call("script.evaluate", &params)?;
                if result.get("type").and_then(Value::as_str) == Some("exception") {
                    let text = result
                        .at(&["exceptionDetails", "text"])
                        .and_then(Value::as_str);
                    return Err(format!(
                        "the page says: {}",
                        text.unwrap_or("the script failed")
                    ));
                }
                let text = result.at(&["result", "value"]).and_then(Value::as_str);
                Value::parse(text.unwrap_or("null"))
            }
        }
    }

    /// Goes to an address and waits until the page has loaded.
    pub fn goto(&mut self, url: &str) -> Result<(), String> {
        match self.context.clone() {
            Some(context) => {
                let params = format!(
                    "{{\"context\":{},\"url\":{},\"wait\":\"complete\"}}",
                    quote(&context),
                    quote(url)
                );
                self.call("browsingContext.navigate", &params).map(drop)
            }
            None => {
                let result = self.call("Page.navigate", &format!("{{\"url\":{}}}", quote(url)))?;
                if let Some(error) = result.get("errorText").and_then(Value::as_str) {
                    return Err(format!("the browser says: {error}"));
                }
                // The new page takes over from the old; ask it until it has loaded.
                let start = std::time::Instant::now();
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    if self.loaded().unwrap_or(false) {
                        return Ok(());
                    }
                    if start.elapsed() > PATIENCE {
                        return Err("the page did not finish loading".into());
                    }
                }
            }
        }
    }

    pub fn loaded(&mut self) -> Result<bool, String> {
        let state = self.eval("document.readyState")?;
        Ok(state.as_str() == Some("complete"))
    }
}

impl Drop for Page {
    /// Firefox takes one session at a time: ours ends with the command, for the next one.
    fn drop(&mut self) {
        if self.context.is_some() {
            let _ = self.call("session.end", "{}");
        }
    }
}

/// The JavaScript behind the commands that name an element by a CSS selector.
pub fn script(command: &str, selector: &str, text: &str) -> Option<String> {
    let find = format!("document.querySelector({})", quote(selector));
    let missing = format!("throw new Error('no element ' + {})", quote(selector));
    Some(match command {
        "text" if selector.is_empty() => "document.body ? document.body.innerText : ''".to_string(),
        "text" => format!(
            "(() => {{ const e = {find}; if (!e) {missing}; return e.innerText ?? e.textContent; }})()"
        ),
        "html" => {
            format!("(() => {{ const e = {find}; if (!e) {missing}; return e.outerHTML; }})()")
        }
        "exists" => format!("{find} !== null"),
        "count" => format!("document.querySelectorAll({}).length", quote(selector)),
        "click" => {
            format!("(() => {{ const e = {find}; if (!e) {missing}; e.click(); return true; }})()")
        }
        "fill" => format!(
            "(() => {{ const e = {find}; if (!e) {missing}; e.focus(); e.value = {}; \
             e.dispatchEvent(new Event('input', {{bubbles: true}})); \
             e.dispatchEvent(new Event('change', {{bubbles: true}})); return true; }})()",
            quote(text)
        ),
        "title" => "document.title".to_string(),
        "url" => "location.href".to_string(),
        _ => return None,
    })
}

/// A value as a command prints it: text as it is, everything else as JSON.
pub fn show(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_json(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A server that takes one WebSocket client and answers each message through `answer`.
    fn serve(answer: fn(&str) -> Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            assert!(String::from_utf8_lossy(&head).contains("Upgrade: websocket"));
            stream
                .write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
                .unwrap();
            loop {
                let mut head = [0; 2];
                if stream.read_exact(&mut head).is_err() {
                    return;
                }
                assert!(head[1] & 0x80 != 0, "a client has to mask");
                let length = match head[1] & 0x7f {
                    126 => {
                        let mut bytes = [0; 2];
                        stream.read_exact(&mut bytes).unwrap();
                        u16::from_be_bytes(bytes) as usize
                    }
                    short => short as usize,
                };
                let mut mask = [0; 4];
                stream.read_exact(&mut mask).unwrap();
                let mut payload = vec![0; length];
                stream.read_exact(&mut payload).unwrap();
                for (i, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[i % 4];
                }
                for text in answer(&String::from_utf8_lossy(&payload)) {
                    // Servers do not mask; long answers come in two pieces.
                    let bytes = text.as_bytes();
                    let (first, second) = bytes.split_at(bytes.len() / 2);
                    for (opcode, piece, last) in [(1u8, first, false), (0, second, true)] {
                        let mut frame = vec![opcode | if last { 0x80 } else { 0 }];
                        match piece.len() {
                            length @ 0..126 => frame.push(length as u8),
                            length => {
                                frame.push(126);
                                frame.extend((length as u16).to_be_bytes());
                            }
                        }
                        frame.extend(piece);
                        stream.write_all(&frame).unwrap();
                    }
                }
            }
        });
        address
    }

    #[test]
    fn messages_travel_both_ways() {
        let address = serve(|text| vec![format!("echo {text}")]);
        let mut socket = Socket::connect(&address, "/x").unwrap();
        socket.send("short").unwrap();
        assert_eq!(socket.receive().unwrap(), "echo short");
        let long = "ä".repeat(500);
        socket.send(&long).unwrap();
        assert_eq!(socket.receive().unwrap(), format!("echo {long}"));
    }

    #[test]
    fn a_devtools_page_evaluates_past_events() {
        let address = serve(|text| {
            let id = Value::parse(text)
                .unwrap()
                .get("id")
                .and_then(Value::as_f64)
                .unwrap();
            let event = "{\"method\":\"Page.frameNavigated\",\"params\":{}}".to_string();
            let result = match text.contains("boom") {
                true => {
                    "{\"exceptionDetails\":{\"text\":\"Uncaught\",\"exception\":{\"description\":\"Error: boom\"}}}"
                }
                false => "{\"result\":{\"type\":\"string\",\"value\":\"Example Domain\"}}",
            };
            vec![event, format!("{{\"id\":{id},\"result\":{result}}}")]
        });
        let mut page = Page {
            socket: Socket::connect(&address, "/devtools/page/1").unwrap(),
            next: 0,
            context: None,
        };
        assert_eq!(
            page.eval("document.title"),
            Ok(Value::String("Example Domain".into()))
        );
        assert_eq!(
            page.eval("boom()"),
            Err("the page says: Error: boom".into())
        );
    }

    #[test]
    fn a_bidi_page_evaluates_through_json_text() {
        let address = serve(|text| {
            let message = Value::parse(text).unwrap();
            let id = message.get("id").and_then(Value::as_f64).unwrap();
            assert_eq!(
                message
                    .at(&["params", "target", "context"])
                    .and_then(Value::as_str),
                Some("tab1")
            );
            let result = "{\"type\":\"success\",\"result\":{\"type\":\"string\",\"value\":\"[1,\\\"two\\\"]\"}}";
            vec![format!(
                "{{\"type\":\"success\",\"id\":{id},\"result\":{result}}}"
            )]
        });
        let mut page = Page {
            socket: Socket::connect(&address, "/session").unwrap(),
            next: 0,
            context: Some("tab1".into()),
        };
        let value = page.eval("[1, 'two']").unwrap();
        assert_eq!(value.to_json(), "[1,\"two\"]");
    }

    #[test]
    fn endpoints_scripts_and_options() {
        let dir = std::env::temp_dir().join(format!("wirbel-web-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(endpoint(&dir).is_err());
        std::fs::write(
            dir.join("DevToolsActivePort"),
            "41669\n/devtools/browser/x\n",
        )
        .unwrap();
        assert_eq!(endpoint(&dir), Ok(Endpoint::Cdp("127.0.0.1:41669".into())));
        std::fs::write(
            dir.join("WebDriverBiDiServer.json"),
            "{\"ws_host\":\"127.0.0.1\",\"ws_port\":45581}",
        )
        .unwrap();
        assert_eq!(endpoint(&dir), Ok(Endpoint::Bidi("127.0.0.1:45581".into())));
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(listen_options("firefox"), Some("--remote-debugging-port 0"));
        assert_eq!(
            listen_options("chromium-browser"),
            Some("--remote-debugging-port=0")
        );
        assert_eq!(listen_options("kitty"), None);
        let click = script("click", "a[href=\"x\"]", "").unwrap();
        assert!(click.contains("document.querySelector(\"a[href=\\\"x\\\"]\")"));
        assert!(
            script("fill", "#q", "it's")
                .unwrap()
                .contains("e.value = \"it's\"")
        );
        assert_eq!(script("title", "", ""), Some("document.title".into()));
        assert_eq!(script("dance", "", ""), None);
        assert_eq!(show(&Value::String("x".into())), "x");
        assert_eq!(show(&Value::Number(3.0)), "3");
        assert_eq!(show(&Value::Null), "");
    }
}
