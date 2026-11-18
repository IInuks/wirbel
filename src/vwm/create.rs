//! `wirbel vwm create macro [NAME]`: makes a macro without writing one. It asks whether a
//! website or a program is to be driven, starts it invisibly, shows it in a viewer on the
//! desktop (`vwm show`) and writes down what the user clicks and types there. The viewer
//! records raw events (`viewer --record FILE`); `steps` turns them into steps of a macro, merging
//! what belongs together (letters into one `type`, two clicks into a double click, BackSpace
//! taking back a letter), and `render` writes the macro with waits after each step that changes
//! the window, so that it does not run ahead of the program when it runs by itself.
//!
//! The questions are in German, the language of the user it was made for.

use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::{cli::capture, valid_name};

/// The size of the window while recording and when the macro runs: clicks go to the same places.
const SIZE: &str = "1280x800";
/// Browsers tried for a website, in this order.
const BROWSERS: &[&str] = &["firefox", "chromium", "google-chrome", "brave", "librewolf"];
/// A press and release closer than this are a click, else a drag.
const CLICK_SLACK: f64 = 8.0;
/// Two clicks closer in time than this are a double click.
const DOUBLE_CLICK: u64 = 400;
/// A pause of the user longer than this means they waited for the program.
const LONG_PAUSE: u64 = 1500;
/// A step is shown as done when nothing was added to it for this long.
const SETTLED: u64 = 800;

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Down(String, f64, f64),
    Up(String, f64, f64),
    Wheel(f64, f64, f64),
    Text(String),
    Key(String),
    /// `z` in the terminal: take back the last step.
    Undo,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub ms: u64,
    pub kind: Kind,
}

/// One line the viewer recorded: `MS down left X Y`, `MS up left X Y`, `MS wheel STEPS X Y`,
/// `MS text CHARACTERS`, `MS key COMBINATION`.
pub fn parse_event(line: &str) -> Option<Event> {
    let (ms, rest) = line.split_once(' ')?;
    let ms = ms.parse().ok()?;
    let (word, rest) = rest.split_once(' ').unwrap_or((rest, ""));
    let numbers =
        |text: &str| -> Option<Vec<f64>> { text.split(' ').map(|w| w.parse().ok()).collect() };
    let kind = match word {
        "down" | "up" => {
            let (button, place) = rest.split_once(' ')?;
            let [x, y] = numbers(place)?[..] else {
                return None;
            };
            match word {
                "down" => Kind::Down(button.to_string(), x, y),
                _ => Kind::Up(button.to_string(), x, y),
            }
        }
        "wheel" => {
            let [steps, x, y] = numbers(rest)?[..] else {
                return None;
            };
            Kind::Wheel(steps, x, y)
        }
        "text" if !rest.is_empty() => Kind::Text(rest.to_string()),
        "key" if !rest.is_empty() => Kind::Key(rest.to_string()),
        _ => return None,
    };
    Some(Event { ms, kind })
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Click {
        x: i64,
        y: i64,
        button: String,
        count: u32,
    },
    Drag {
        from: (i64, i64),
        to: (i64, i64),
        button: String,
    },
    Scroll {
        steps: f64,
        x: i64,
        y: i64,
    },
    Type(String),
    Key(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub action: Action,
    /// When it began and when the last event of it came, in milliseconds.
    pub start: u64,
    pub end: u64,
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn place(x: f64, y: f64) -> (i64, i64) {
    (x.round() as i64, y.round() as i64)
}

/// The steps the events make, in order.
pub fn steps(events: &[Event]) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let mut held: HashMap<String, (f64, f64, u64)> = HashMap::new();
    for event in events {
        let ms = event.ms;
        match &event.kind {
            Kind::Down(button, x, y) => {
                held.insert(button.clone(), (*x, *y, ms));
            }
            Kind::Up(button, x, y) => {
                let Some((dx, dy, down)) = held.remove(button) else {
                    continue;
                };
                if distance((dx, dy), (*x, *y)) > CLICK_SLACK {
                    steps.push(Step {
                        action: Action::Drag {
                            from: place(dx, dy),
                            to: place(*x, *y),
                            button: button.clone(),
                        },
                        start: down,
                        end: ms,
                    });
                    continue;
                }
                let (cx, cy) = place(dx, dy);
                if let Some(last) = steps.last_mut()
                    && let Action::Click {
                        x: lx,
                        y: ly,
                        button: lb,
                        count,
                    } = &mut last.action
                    && *count == 1
                    && lb == button
                    && down.saturating_sub(last.end) <= DOUBLE_CLICK
                    && distance((*lx as f64, *ly as f64), (dx, dy)) <= CLICK_SLACK
                {
                    *count = 2;
                    last.end = ms;
                    continue;
                }
                steps.push(Step {
                    action: Action::Click {
                        x: cx,
                        y: cy,
                        button: button.clone(),
                        count: 1,
                    },
                    start: down,
                    end: ms,
                });
            }
            Kind::Wheel(amount, x, y) => {
                if let Some(last) = steps.last_mut()
                    && let Action::Scroll {
                        steps: total,
                        x: lx,
                        y: ly,
                    } = &mut last.action
                    && ms.saturating_sub(last.end) <= 1000
                    && distance((*lx as f64, *ly as f64), (*x, *y)) <= 40.0
                {
                    *total += amount;
                    last.end = ms;
                    continue;
                }
                let (x, y) = place(*x, *y);
                steps.push(Step {
                    action: Action::Scroll {
                        steps: *amount,
                        x,
                        y,
                    },
                    start: ms,
                    end: ms,
                });
            }
            Kind::Text(text) => {
                if let Some(last) = steps.last_mut()
                    && let Action::Type(typed) = &mut last.action
                {
                    typed.push_str(text);
                    last.end = ms;
                    continue;
                }
                steps.push(Step {
                    action: Action::Type(text.clone()),
                    start: ms,
                    end: ms,
                });
            }
            Kind::Key(key) => {
                // A typo fixed while typing: the macro types it right at once.
                if key == "BackSpace"
                    && let Some(last) = steps.last_mut()
                    && let Action::Type(typed) = &mut last.action
                {
                    typed.pop();
                    last.end = ms;
                    if typed.is_empty() {
                        steps.pop();
                    }
                    continue;
                }
                steps.push(Step {
                    action: Action::Key(key.clone()),
                    start: ms,
                    end: ms,
                });
            }
            Kind::Undo => {
                steps.pop();
            }
        }
    }
    steps
}

/// The scroll steps as whole notches, at least one.
fn notches(steps: f64) -> i64 {
    match steps.round() as i64 {
        0 if steps < 0.0 => -1,
        0 => 1,
        n => n,
    }
}

/// A step in words, for the terminal.
pub fn describe(action: &Action) -> String {
    let side = |button: &str| match button {
        "left" => String::new(),
        "right" => " (rechte Taste)".into(),
        "middle" => " (mittlere Taste)".into(),
        other => format!(" (Taste {other})"),
    };
    match action {
        Action::Click {
            x,
            y,
            button,
            count,
        } => {
            let what = if *count == 2 { "Doppelklick" } else { "Klick" };
            format!("{what} bei {x}, {y}{}", side(button))
        }
        Action::Drag { from, to, button } => format!(
            "Ziehen von {}, {} nach {}, {}{}",
            from.0,
            from.1,
            to.0,
            to.1,
            side(button)
        ),
        Action::Scroll { steps, .. } => {
            let n = notches(*steps);
            let way = if n > 0 { "unten" } else { "oben" };
            format!("{}× nach {way} scrollen", n.abs())
        }
        Action::Type(text) => format!("Tippen: „{text}“"),
        Action::Key(key) => format!("Taste {key}"),
    }
}

/// A text as a quoted word of a macro.
pub fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '$' => out.push_str("$$"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What the macro opens.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Web { browser: String, url: String },
    Program(String),
}

/// The macro for these steps. `name` is the macro and its window.
pub fn render(name: &str, target: &Target, steps: &[Step], date: &str) -> String {
    let mut out = String::new();
    let mut line = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };
    line(&format!(
        "# {name}: aufgenommen mit `wirbel vwm create macro` am {date}."
    ));
    line(&format!(
        "#   wirbel macro run {name}              läuft unsichtbar"
    ));
    line(&format!(
        "#   wirbel macro run {name} show=yes     zeigt dabei ein Fenster zum Zuschauen"
    ));
    line(&format!(
        "#   wirbel macro run {name} close=no     lässt das Fenster am Ende offen"
    ));
    match target {
        Target::Web { .. } => line(&format!(
            "#   wirbel macro run {name} url=LINK     auf einer anderen Seite"
        )),
        Target::Program(_) => line(&format!(
            "#   wirbel macro run {name} app=BEFEHL   mit einem anderen Programm"
        )),
    }
    line("");
    match target {
        Target::Web { url, .. } => line(&format!("param url = {}", quote(url))),
        Target::Program(command) => line(&format!("param app = {}", quote(command))),
    }
    line("param show = \"no\"");
    line("param close = \"yes\"");
    line("");
    line("# Wartet, bis sich das Bild nicht mehr ändert; läuft dort etwas dauernd, geht es nach");
    line("# der Zeit trotzdem weiter.");
    line("func settle(ms, limit)");
    line("    try");
    line(&format!("        wait {name} stable $ms timeout $limit"));
    line("    end");
    line("end");
    line("");
    line("# Ein Fenster vom letzten Mal schließen.");
    line("try");
    line(&format!("    close {name}"));
    line("end");
    match target {
        Target::Web { browser, .. } => {
            line(&format!("open {name} = --size {SIZE} {browser} \"$url\""))
        }
        Target::Program(_) => line(&format!("open {name} = --size {SIZE} \"$app\"")),
    }
    line("if $show == \"yes\"");
    line("    try");
    line(&format!("        shell \"wirbel vwm show {name}\""));
    line("    end");
    line("end");
    line(&format!("wait {name} ready timeout 30s"));
    line("settle(800, 30)");
    for (index, step) in steps.iter().enumerate() {
        line("");
        line(&format!("# {}: {}", index + 1, describe(&step.action)));
        let command = match &step.action {
            Action::Click {
                x,
                y,
                button,
                count,
            } => match (button.as_str(), count) {
                ("left", 1) => format!("click {name} {x} {y}"),
                (button, count) => format!("click {name} {x} {y} {button} {count}"),
            },
            Action::Drag { from, to, button } => match button.as_str() {
                "left" => format!("drag {name} {} {} {} {}", from.0, from.1, to.0, to.1),
                button => format!(
                    "drag {name} {} {} {} {} {button}",
                    from.0, from.1, to.0, to.1
                ),
            },
            Action::Scroll { steps, x, y } => {
                format!("scroll {name} {} {x} {y}", notches(*steps))
            }
            Action::Type(text) => format!("type {name} {}", quote(text)),
            Action::Key(key) => format!("key {name} {key}"),
        };
        line(&command);
        // Wait as the user did: a long pause was waiting for the program, and after anything
        // but typing the program may draw something new before the next step can find it.
        let pause = steps
            .get(index + 1)
            .map(|next| next.start.saturating_sub(step.end));
        let typing = matches!(step.action, Action::Type(_));
        match pause {
            Some(pause) if pause > LONG_PAUSE => {
                let limit = (pause * 4 / 1000).clamp(15, 60);
                line(&format!("settle(500, {limit})"));
            }
            Some(_) if !typing => line("settle(250, 10)"),
            None => line("settle(500, 15)"),
            Some(_) => {}
        }
    }
    line("");
    line("if $close == \"yes\"");
    line(&format!("    close {name}"));
    line("end");
    out
}

/// A name for the macro from a link or a command: `example` from `https://www.example.com/x`.
pub fn suggest_name(target: &Target) -> String {
    let raw = match target {
        Target::Web { url, .. } => {
            let host = url.split("://").nth(1).unwrap_or(url);
            let host = host.split(['/', '?', '#', ':']).next().unwrap_or("");
            let host = host.strip_prefix("www.").unwrap_or(host);
            host.split('.').next().unwrap_or("").to_string()
        }
        Target::Program(command) => {
            let program = command.split_whitespace().next().unwrap_or("");
            program.rsplit('/').next().unwrap_or("").to_string()
        }
    };
    let cleaned: String = raw
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' | '_' => c,
            _ => '-',
        })
        .collect();
    let cleaned: String = cleaned.trim_matches('-').chars().take(32).collect();
    match valid_name(&cleaned) {
        true => cleaned,
        false => "makro".into(),
    }
}

/// A link as the browser wants it: `example.com` becomes `https://example.com`.
pub fn normalise_url(text: &str) -> String {
    let text = text.trim();
    match text.contains("://") {
        true => text.to_string(),
        false => format!("https://{text}"),
    }
}

/// Whether `program` can be started: a path to a file, or a name found in PATH.
fn runnable(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_millis() as u64)
}

fn today() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "heute".into())
}

/// The terminal: one thread reads its lines, so that the recording can listen while it waits.
struct Terminal {
    lines: Receiver<String>,
}

impl Terminal {
    fn new() -> Self {
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::stdin().lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self { lines }
    }

    fn ask(&self, question: &str) -> Result<String, String> {
        print!("{question} ");
        let _ = std::io::stdout().flush();
        self.lines
            .recv()
            .map(|line| line.trim().to_string())
            .map_err(|_| "keine Eingabe mehr, abgebrochen".into())
    }

    /// A yes or no question; Enter alone answers `default`.
    fn yes(&self, question: &str, default: bool) -> Result<bool, String> {
        let hint = if default { "[J/n]" } else { "[j/N]" };
        loop {
            let answer = self.ask(&format!("{question} {hint}"))?.to_lowercase();
            match answer.as_str() {
                "" => return Ok(default),
                "j" | "ja" | "y" | "yes" => return Ok(true),
                "n" | "nein" | "no" => return Ok(false),
                _ => println!("Bitte j oder n."),
            }
        }
    }
}

fn ask_target(terminal: &Terminal) -> Result<Target, String> {
    println!("Was soll das Makro öffnen?");
    println!("  1) eine Website");
    println!("  2) ein Programm");
    let web = loop {
        match terminal.ask(">")?.to_lowercase().as_str() {
            "1" | "w" | "web" | "website" => break true,
            "2" | "p" | "programm" | "program" => break false,
            _ => println!("Bitte 1 oder 2 eingeben."),
        }
    };
    if web {
        let browser = BROWSERS
            .iter()
            .find(|name| runnable(name))
            .ok_or("kein Browser gefunden (firefox oder chromium)")?;
        loop {
            let link = terminal.ask("Link der Website (z. B. example.com):")?;
            if link.is_empty() || link.contains(char::is_whitespace) {
                println!("Bitte einen Link ohne Leerzeichen eingeben.");
                continue;
            }
            return Ok(Target::Web {
                browser: browser.to_string(),
                url: normalise_url(&link),
            });
        }
    }
    loop {
        let command = terminal.ask("Welches Programm soll starten (z. B. gnome-calculator)?")?;
        let Some(program) = command.split_whitespace().next() else {
            continue;
        };
        if runnable(program) {
            return Ok(Target::Program(command));
        }
        println!("„{program}“ gibt es auf diesem Rechner nicht. Bitte nochmal.");
    }
}

fn ask_name(terminal: &Terminal, given: Option<&str>, target: &Target) -> Result<String, String> {
    let library = crate::macros::library().ok_or("kann den Konfigurationsordner nicht finden")?;
    let mut given = given.map(str::to_string);
    loop {
        let name = match given.take() {
            Some(name) => name,
            None => {
                let suggestion = suggest_name(target);
                let answer = terminal.ask(&format!("Name des Makros [{suggestion}]:"))?;
                if answer.is_empty() {
                    suggestion
                } else {
                    answer
                }
            }
        };
        if !valid_name(&name) {
            println!("Der Name darf nur Buchstaben, Ziffern, - und _ haben, höchstens 32.");
            continue;
        }
        if library.join(format!("{name}.wm")).is_file()
            && !terminal.yes(
                &format!("Ein Makro „{name}“ gibt es schon. Ersetzen?"),
                false,
            )?
        {
            continue;
        }
        if capture(&["info".into(), name.clone()]).is_ok() {
            if !terminal.yes(
                &format!("Ein unsichtbares Fenster „{name}“ läuft gerade. Schließen und weiter?"),
                true,
            )? {
                continue;
            }
            capture(&["close".into(), name.clone()])?;
        }
        return Ok(name);
    }
}

/// Starts the window invisibly and waits until it shows something.
fn open(name: &str, target: &Target) -> Result<(), String> {
    let mut args: Vec<String> = ["open", "--name", name, "--size", SIZE]
        .map(String::from)
        .to_vec();
    match target {
        Target::Web { browser, url } => {
            args.push(browser.clone());
            args.push(url.clone());
        }
        Target::Program(command) => args.push(command.clone()),
    }
    capture(&args)?;
    capture(&["wait", name, "ready", "--timeout", "30"].map(String::from))?;
    // A page that never settles is not an error here.
    let _ = capture(&["wait", name, "stable", "800", "--timeout", "20"].map(String::from));
    Ok(())
}

fn record_file(name: &str) -> Result<PathBuf, String> {
    let dir = super::manager::state_dir(name).ok_or("kann den Statusordner nicht finden")?;
    std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    Ok(dir.join("record.log"))
}

/// Shows the window and writes down what happens in it until the user is done.
fn record(terminal: &Terminal, name: &str) -> Result<Vec<Step>, String> {
    let file = record_file(name)?;
    let _ = std::fs::remove_file(&file);
    std::fs::write(&file, "").map_err(|err| format!("{}: {err}", file.display()))?;
    let log = file.with_file_name("viewer.log");
    let log = std::fs::File::create(&log).map_err(|err| format!("{}: {err}", log.display()))?;
    let exe = std::env::current_exe().map_err(|err| format!("finde wirbel nicht: {err}"))?;
    let mut viewer = std::process::Command::new(exe)
        .args(["vwm", "viewer", name, "--record"])
        .arg(&file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .spawn()
        .map_err(|err| format!("das Fenster startet nicht: {err}"))?;

    println!();
    println!("Ein Fenster zeigt es jetzt. Klick hinein und mach deine Aktionen nacheinander:");
    println!("klicken, tippen, Tasten, scrollen. Jede wird hier mitgeschrieben.");
    println!("Warte ruhig, wenn etwas lädt: das Makro wartet dann an der Stelle auch.");
    println!();
    println!("Hier im Terminal:  Enter = fertig    z + Enter = letzten Schritt löschen");
    println!();

    let mut own: Vec<Event> = Vec::new();
    let mut printed = 0;
    let mut current: Vec<Step> = Vec::new();
    let mut ended_by_window = false;
    loop {
        match terminal.lines.recv_timeout(Duration::from_millis(150)) {
            Ok(line) => match line.trim().to_lowercase().as_str() {
                "" | "f" | "fertig" => break,
                "z" => {
                    own.push(Event {
                        ms: now_ms(),
                        kind: Kind::Undo,
                    });
                    println!("  ↶ letzter Schritt gelöscht");
                }
                _ => println!("  (Enter = fertig, z = letzten Schritt löschen)"),
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if viewer.try_wait().ok().flatten().is_some() {
            ended_by_window = true;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let mut events: Vec<Event> = text.lines().filter_map(parse_event).collect();
        events.extend(own.iter().cloned());
        events.sort_by_key(|event| event.ms);
        current = steps(&events);
        let settled = match current.last() {
            Some(last) if last.end + SETTLED > now_ms() && !ended_by_window => current.len() - 1,
            _ => current.len(),
        };
        printed = printed.min(settled);
        for (index, step) in current.iter().enumerate().take(settled).skip(printed) {
            println!("  {:>2}. {}", index + 1, describe(&step.action));
        }
        printed = settled;
        if ended_by_window {
            println!("Das Fenster wurde geschlossen, die Aufnahme ist zu Ende.");
            break;
        }
    }
    for (index, step) in current.iter().enumerate().skip(printed) {
        println!("  {:>2}. {}", index + 1, describe(&step.action));
    }
    let _ = viewer.kill();
    let _ = viewer.wait();
    let _ = std::fs::remove_file(&file);
    Ok(current)
}

/// `wirbel vwm create macro [NAME]`.
pub fn wizard(args: &[String]) -> Result<(), String> {
    let given = match args {
        [] => None,
        [name] => Some(name.as_str()),
        _ => return Err("usage: wirbel vwm create macro [NAME]".into()),
    };
    if !cfg!(feature = "vwm-viewer") {
        return Err("this wirbel was built without the viewer (feature vwm-viewer)".into());
    }
    let terminal = Terminal::new();
    println!("Neues Makro");
    println!();
    let target = ask_target(&terminal)?;
    let name = ask_name(&terminal, given, &target)?;
    match &target {
        Target::Web { browser, url } => println!("Öffne {url} mit {browser} …"),
        Target::Program(command) => println!("Starte {command} …"),
    }
    open(&name, &target)?;
    let recorded = record(&terminal, &name);
    let _ = capture(&["close".into(), name.clone()]);
    let steps = recorded?;

    println!();
    if steps.is_empty()
        && !terminal.yes("Es wurde nichts aufgenommen. Trotzdem speichern?", false)?
    {
        println!("Nichts gespeichert.");
        return Ok(());
    }
    let source = render(&name, &target, &steps, &today());
    if let Err(err) = crate::macros::syntax::parse(&source, super::cli::COMMANDS) {
        return Err(format!(
            "das Makro ist fehlerhaft geworden (Zeile {}: {}), bitte melden",
            err.line, err.message
        ));
    }
    let library = crate::macros::library().ok_or("kann den Konfigurationsordner nicht finden")?;
    std::fs::create_dir_all(&library).map_err(|err| format!("{}: {err}", library.display()))?;
    let path = library.join(format!("{name}.wm"));
    std::fs::write(&path, &source).map_err(|err| format!("{}: {err}", path.display()))?;
    println!("Gespeichert: {} ({} Schritte)", path.display(), steps.len());
    println!("Starten mit:  wirbel macro run {name}");
    println!("Zuschauen:    wirbel macro run {name} show=yes");
    println!();
    if terminal.yes("Jetzt einmal zum Zuschauen abspielen?", true)? {
        let code = crate::macros::main(&["run".into(), name.clone(), "show=yes".into()]);
        match code {
            0 => println!("Fertig, das Makro lief durch."),
            _ => println!("Das Makro ist hängen geblieben (siehe oben)."),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(lines: &[&str]) -> Vec<Event> {
        lines
            .iter()
            .map(|line| parse_event(line).unwrap())
            .collect()
    }

    fn actions(lines: &[&str]) -> Vec<Action> {
        steps(&events(lines))
            .into_iter()
            .map(|s| s.action)
            .collect()
    }

    #[test]
    fn events_read_back() {
        assert_eq!(
            parse_event("5 down left 10 20"),
            Some(Event {
                ms: 5,
                kind: Kind::Down("left".into(), 10.0, 20.0)
            })
        );
        assert_eq!(
            parse_event("5 text  ").unwrap().kind,
            Kind::Text(" ".into())
        );
        assert_eq!(
            parse_event("5 key CTRL+l").unwrap().kind,
            Kind::Key("CTRL+l".into())
        );
        assert_eq!(parse_event("garbage"), None);
        assert_eq!(parse_event("5 down left 10"), None);
    }

    #[test]
    fn press_and_release_make_clicks_double_clicks_and_drags() {
        assert_eq!(
            actions(&["0 down left 10 10", "50 up left 12 11"]),
            [Action::Click {
                x: 10,
                y: 10,
                button: "left".into(),
                count: 1
            }]
        );
        assert_eq!(
            actions(&[
                "0 down left 10 10",
                "50 up left 10 10",
                "200 down left 11 10",
                "250 up left 11 10",
            ]),
            [Action::Click {
                x: 10,
                y: 10,
                button: "left".into(),
                count: 2
            }]
        );
        // Too far apart in time: two clicks.
        assert_eq!(
            actions(&[
                "0 down left 10 10",
                "50 up left 10 10",
                "900 down left 10 10",
                "950 up left 10 10",
            ])
            .len(),
            2
        );
        assert_eq!(
            actions(&["0 down left 10 10", "300 up left 200 100"]),
            [Action::Drag {
                from: (10, 10),
                to: (200, 100),
                button: "left".into()
            }]
        );
    }

    #[test]
    fn letters_join_and_backspace_takes_them_back() {
        assert_eq!(
            actions(&[
                "0 text h",
                "10 text a",
                "20 text x",
                "30 key BackSpace",
                "40 text l",
                "50 text l",
                "60 text o",
                "70 key Return",
            ]),
            [Action::Type("hallo".into()), Action::Key("Return".into())]
        );
        // BackSpace with nothing typed before stays a key.
        assert_eq!(
            actions(&["0 key BackSpace"]),
            [Action::Key("BackSpace".into())]
        );
        assert_eq!(actions(&["0 text a", "10 key BackSpace"]), []);
    }

    #[test]
    fn wheel_turns_add_up_and_undo_drops_the_last_step() {
        assert_eq!(
            actions(&[
                "0 wheel 1 50 50",
                "100 wheel 1.5 52 50",
                "200 wheel 0.5 50 50"
            ]),
            [Action::Scroll {
                steps: 3.0,
                x: 50,
                y: 50
            }]
        );
        let mut list = events(&["0 text a", "100 key Return"]);
        list.push(Event {
            ms: 200,
            kind: Kind::Undo,
        });
        assert_eq!(
            steps(&list)
                .into_iter()
                .map(|s| s.action)
                .collect::<Vec<_>>(),
            [Action::Type("a".into())]
        );
    }

    #[test]
    fn names_and_links() {
        let web = |url: &str| Target::Web {
            browser: "firefox".into(),
            url: url.into(),
        };
        assert_eq!(suggest_name(&web("https://www.example.com/a")), "example");
        assert_eq!(suggest_name(&web("https://open.spotify.com")), "open");
        assert_eq!(
            suggest_name(&Target::Program("/usr/bin/gnome-calculator -x".into())),
            "gnome-calculator"
        );
        assert_eq!(suggest_name(&Target::Program("ÄÖÜ".into())), "makro");
        assert_eq!(normalise_url(" example.com "), "https://example.com");
        assert_eq!(normalise_url("http://x.org"), "http://x.org");
        assert_eq!(quote("a \"b\" $c \\"), "\"a \\\"b\\\" $$c \\\\\"");
    }

    #[test]
    fn the_macro_parses_and_waits_where_the_user_waited() {
        let steps = steps(&events(&[
            "0 down left 100 200",
            "50 up left 100 200",
            "300 text hallo $welt",
            "400 key Return",
            "5000 wheel 2 10 10",
        ]));
        let target = Target::Web {
            browser: "firefox".into(),
            url: "https://example.com/?a=1&b=2".into(),
        };
        let source = render("test", &target, &steps, "2026-11-18");
        crate::macros::syntax::parse(&source, super::super::cli::COMMANDS).unwrap();
        assert!(source.contains("click test 100 200\nsettle(250, 10)\n"));
        assert!(source.contains("type test \"hallo $$welt\"\n\n"));
        assert!(source.contains("key test Return\nsettle(500, 18)\n"));
        assert!(source.contains("scroll test 2 10 10\nsettle(500, 15)\n"));
        let program = render(
            "calc",
            &Target::Program("gnome-calculator".into()),
            &[],
            "x",
        );
        crate::macros::syntax::parse(&program, super::super::cli::COMMANDS).unwrap();
    }
}
