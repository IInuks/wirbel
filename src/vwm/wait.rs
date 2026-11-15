//! Waiting until an invisible window is in some state: `wirbel vwm wait NAME CONDITION` and the
//! `wait` of macros. The waiter asks the shadow for its status a few dozen times a second and
//! looks at its picture only when the picture changed, so waiting costs next to nothing.
//!
//! Conditions:
//!
//! - `ready`: the program has a window that has drawn.
//! - `window [GLOB]`: a window is open, one whose title or app id matches GLOB if given.
//! - `title GLOB`: the window with the keyboard has such a title.
//! - `closed`: no window is open. `exited`: the program has ended; answers its exit code.
//! - `stable [MS]`: the picture has not changed for MS milliseconds, 500 if not given.
//! - `changed [X,Y,W,H]`: the picture, or that part of it, is no longer what it was when the
//!   wait began.
//! - `pixel X Y #RRGGBB [TOLERANCE]`: that pixel has that colour, each part within TOLERANCE
//!   (8 if not given).
//! - `image FILE.png [SIMILARITY]`: the picture in FILE shows somewhere in the window, with at
//!   least that share of its pixels alike (0.95 if not given). Answers the middle of where.
//! - `text GLOB`: a line of text in the window matches, as far as the program `tesseract` can
//!   read it from the picture. Answers the middle of the line and the line.
//! - `loaded`: the document in the window is no longer loading. The program is asked through
//!   the accessibility interface; where it has none, the window counts as loaded when its
//!   picture has rested for 0.8 s and the pointer is no busy one. Answers which of the two.
//! - `cursor NAME`: the pointer has that shape, such as `text`, `pointer`, `wait`, `default`.
//! - `clipboard`: something new was copied.
//! - `file PATH`: that file exists.
//!
//! A GLOB matches the whole text, `*` stands for any text and `?` for one character; without
//! either it matches when the text holds it.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use super::{
    cli::{Scratch, Session},
    json::Value,
    shot::{parse_region, read_png},
};

/// How often the status is asked.
const POLL: Duration = Duration::from_millis(40);
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_STABLE: Duration = Duration::from_millis(500);
const DEFAULT_TOLERANCE: u8 = 8;
/// How long the picture rests before a window without accessibility counts as loaded.
const LOADED_QUIET: Duration = Duration::from_millis(800);
const DEFAULT_SIMILARITY: f64 = 0.95;
/// How far apart the parts of two pixels may be to count as alike in an image search.
const ALIKE: u8 = 24;

/// What `shadow status` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub damage: u64,
    pub quiet: Duration,
    pub windows: usize,
    /// The exit code of the program, once it has ended.
    pub exited: Option<i32>,
    pub cursor: String,
    pub clipboard: u64,
    pub title: String,
}

impl Status {
    /// The answer without its `status ` is fine too.
    pub fn parse(line: &str) -> Result<Self, String> {
        let bad = || format!("unexpected status `{line}`");
        let line = line.strip_prefix("status ").unwrap_or(line);
        let mut fields = line.splitn(7, ' ');
        let mut next = || fields.next().ok_or_else(bad);
        let damage = next()?.parse().map_err(|_| bad())?;
        let quiet = Duration::from_millis(next()?.parse().map_err(|_| bad())?);
        let windows = next()?.parse().map_err(|_| bad())?;
        let exited = match next()? {
            "running" => None,
            other => {
                let code = other.strip_prefix("exited:").ok_or_else(bad)?;
                Some(code.parse().map_err(|_| bad())?)
            }
        };
        let cursor = next()?.to_string();
        let clipboard = next()?.parse().map_err(|_| bad())?;
        let title = fields.next().unwrap_or_default().to_string();
        Ok(Self {
            damage,
            quiet,
            windows,
            exited,
            cursor,
            clipboard,
            title,
        })
    }

    /// For people, in one line.
    pub fn describe(&self) -> String {
        let program = match self.exited {
            Some(code) => format!("program exited with {code}"),
            None => "program running".to_string(),
        };
        format!(
            "{} window(s), title `{}`, {program}, cursor {}, picture quiet for {} ms",
            self.windows,
            self.title,
            self.cursor,
            self.quiet.as_millis()
        )
    }
}

/// See the module documentation.
pub fn glob(pattern: &str, text: &str) -> bool {
    if !pattern.contains(['*', '?']) {
        return text.contains(pattern);
    }
    let (pattern, text): (Vec<char>, Vec<char>) =
        (pattern.chars().collect(), text.chars().collect());
    // Where the last star was, and how much of the text it has taken so far.
    let (mut p, mut t, mut star) = (0, 0, None);
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(c) if *c == '?' || *c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((star_p, star_t)) => {
                    p = star_p + 1;
                    t = star_t + 1;
                    star = Some((star_p, star_t + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    Ready,
    Window(Option<String>),
    Title(String),
    Closed,
    Exited,
    Stable(Duration),
    Changed(Option<(i32, i32, i32, i32)>),
    Pixel {
        x: i32,
        y: i32,
        color: [u8; 3],
        tolerance: u8,
    },
    Image {
        path: PathBuf,
        similarity: f64,
    },
    Cursor(String),
    Clipboard,
    File(PathBuf),
    /// A line of text that text recognition finds in the picture.
    Text(String),
    /// The document in the window has loaded.
    Loaded,
}

/// `#RRGGBB`, with or without the `#`.
pub fn parse_color(text: &str) -> Result<[u8; 3], String> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    let value = u32::from_str_radix(hex, 16).ok().filter(|_| hex.len() == 6);
    let value = value.ok_or_else(|| format!("expected a colour #RRGGBB, got `{text}`"))?;
    Ok([(value >> 16) as u8, (value >> 8) as u8, value as u8])
}

impl Condition {
    pub fn parse(words: &[&str]) -> Result<Self, String> {
        let number = |text: &str| {
            let parsed = text.parse::<i32>().ok().filter(|n| *n >= 0);
            parsed.ok_or_else(|| format!("expected a number, got `{text}`"))
        };
        Ok(match words {
            ["ready"] => Self::Ready,
            ["window"] => Self::Window(None),
            ["window", pattern @ ..] => Self::Window(Some(pattern.join(" "))),
            ["title", pattern @ ..] if !pattern.is_empty() => Self::Title(pattern.join(" ")),
            ["closed"] => Self::Closed,
            ["exited"] => Self::Exited,
            ["stable"] => Self::Stable(DEFAULT_STABLE),
            ["stable", ms] => Self::Stable(Duration::from_millis(number(ms)? as u64)),
            ["changed"] => Self::Changed(None),
            ["changed", region] => Self::Changed(Some(parse_region(region)?)),
            ["pixel", x, y, color] => Self::Pixel {
                x: number(x)?,
                y: number(y)?,
                color: parse_color(color)?,
                tolerance: DEFAULT_TOLERANCE,
            },
            ["pixel", x, y, color, tolerance] => Self::Pixel {
                x: number(x)?,
                y: number(y)?,
                color: parse_color(color)?,
                tolerance: number(tolerance)?.min(255) as u8,
            },
            ["image", path] => Self::Image {
                path: path.into(),
                similarity: DEFAULT_SIMILARITY,
            },
            ["image", path, similarity] => {
                let share = similarity
                    .parse::<f64>()
                    .ok()
                    .filter(|s| *s > 0.0 && *s <= 1.0);
                Self::Image {
                    path: path.into(),
                    similarity: share.ok_or("the similarity is a number above 0, at most 1")?,
                }
            }
            ["text", pattern @ ..] if !pattern.is_empty() => Self::Text(pattern.join(" ")),
            ["loaded"] => Self::Loaded,
            ["cursor", name] => Self::Cursor(name.to_string()),
            ["clipboard"] => Self::Clipboard,
            ["file", path @ ..] if !path.is_empty() => Self::File(path.join(" ").into()),
            _ => {
                return Err(
                    "expected a condition: ready, window [GLOB], title GLOB, closed, exited, \
                     stable [MS], changed [X,Y,W,H], pixel X Y #RRGGBB [TOLERANCE], \
                     image FILE [SIMILARITY], text GLOB, loaded, cursor NAME, clipboard or file PATH"
                        .into(),
                );
            }
        })
    }

    /// Whether the end of the shadow fulfils it.
    fn met_when_gone(&self) -> bool {
        matches!(self, Self::Closed | Self::Exited)
    }
}

/// A picture as rows of RGB pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    pub rgb: Vec<u8>,
    pub width: i32,
    pub height: i32,
}

impl Picture {
    pub fn pixel(&self, x: i32, y: i32) -> Option<[u8; 3]> {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return None;
        }
        let at = (y * self.width + x) as usize * 3;
        Some([self.rgb[at], self.rgb[at + 1], self.rgb[at + 2]])
    }

    /// The pixels of a part of it, row by row. What is outside counts as nothing.
    pub fn part(&self, (x, y, w, h): (i32, i32, i32, i32)) -> Vec<[u8; 3]> {
        let rows = (y..y + h).flat_map(|row| (x..x + w).map(move |column| (column, row)));
        rows.filter_map(|(column, row)| self.pixel(column, row))
            .collect()
    }

    /// Where `small` shows in this picture: its top left corner. `similarity` is the share of
    /// its pixels that have to be alike.
    pub fn find(&self, small: &Picture, similarity: f64) -> Option<(i32, i32)> {
        if small.width < 1
            || small.height < 1
            || small.width > self.width
            || small.height > self.height
        {
            return None;
        }
        let total = (small.width * small.height) as usize;
        let allowed = ((1.0 - similarity) * total as f64).floor() as usize;
        // A few pixels spread over the small picture rule out most places quickly.
        let probes: Vec<(i32, i32)> = (0..24)
            .map(|n| ((n * 37 + 11) % small.width, (n * 53 + 7) % small.height))
            .collect();
        let allowed_probes = ((1.0 - similarity) * probes.len() as f64).ceil() as usize;
        let alike = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(a, b)| a.abs_diff(b) <= ALIKE);
        for y in 0..=self.height - small.height {
            'place: for x in 0..=self.width - small.width {
                let mut missed = 0;
                for (px, py) in &probes {
                    if !alike(self.pixel(x + px, y + py)?, small.pixel(*px, *py)?) {
                        missed += 1;
                        if missed > allowed_probes {
                            continue 'place;
                        }
                    }
                }
                let mut missed = 0;
                for sy in 0..small.height {
                    for sx in 0..small.width {
                        if !alike(self.pixel(x + sx, y + sy)?, small.pixel(sx, sy)?) {
                            missed += 1;
                            if missed > allowed {
                                continue 'place;
                            }
                        }
                    }
                }
                return Some((x, y));
            }
        }
        None
    }

    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let (rgb, width, height) = read_png(path)?;
        Ok(Self {
            rgb,
            width: width as i32,
            height: height as i32,
        })
    }
}

/// `#rrggbb`.
pub fn color_name(color: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitError {
    /// Time ran out; what was seen last.
    Timeout(String),
    Failed(String),
}

impl WaitError {
    pub fn message(&self) -> String {
        match self {
            Self::Timeout(seen) => format!("timeout: {seen}"),
            Self::Failed(err) => err.clone(),
        }
    }
}

/// An invisible window someone looks at for a while.
pub struct Watcher {
    session: Session,
    scratch: Scratch,
    /// The picture, and the damage count it was taken at.
    picture: Option<(u64, Picture)>,
    /// The lines of text read from the picture, and the damage count they are of.
    lines: Option<(u64, Vec<super::text::Line>)>,
    display: String,
    /// The accessibility bus of the window: not tried yet, there, or not to be had.
    tree: Option<Option<super::a11y::Tree>>,
}

impl Watcher {
    pub fn of(name: &str) -> Result<Self, String> {
        Self::on(&super::cli::display_of(name)?)
    }

    /// Of the shadow on this Wayland display.
    pub fn on(display: &str) -> Result<Self, String> {
        Ok(Self {
            session: Session::on(display)?,
            scratch: Scratch::new("shot.png")?,
            picture: None,
            lines: None,
            display: display.to_string(),
            tree: None,
        })
    }

    pub fn status(&mut self) -> Result<Status, String> {
        Status::parse(&self.session.ask("shadow status")?)
    }

    /// The picture of the window as of `status`, taken anew only if it changed since.
    pub fn picture(&mut self, status: &Status) -> Result<&Picture, String> {
        let fresh = matches!(&self.picture, Some((damage, _)) if *damage == status.damage);
        if !fresh {
            let request = format!("shadow shot\t{}", self.scratch.path());
            self.session.ask(&request)?;
            let picture = Picture::load(self.scratch.file())?;
            self.picture = Some((status.damage, picture));
        }
        Ok(&self.picture.as_ref().expect("just taken").1)
    }

    /// The lines of text in the window as of `status`, read anew only if the picture changed.
    pub fn lines(&mut self, status: &Status) -> Result<&[super::text::Line], String> {
        let fresh = matches!(&self.lines, Some((damage, _)) if *damage == status.damage);
        if !fresh {
            self.picture(status)?;
            let lines = super::text::recognise(self.scratch.file(), None)?;
            self.lines = Some((status.damage, lines));
        }
        Ok(&self.lines.as_ref().expect("just read").1)
    }

    /// Whether the documents in the window have loaded, if the programs say.
    pub fn documents_loaded(&mut self) -> Option<bool> {
        let display = &self.display;
        let tree = self
            .tree
            .get_or_insert_with(|| super::a11y::Tree::of(display).ok());
        super::a11y::loaded(&tree.as_ref()?.items().ok()?)
    }

    /// The titles and app ids of the windows.
    fn windows(&mut self) -> Result<Vec<(String, String)>, String> {
        let windows = Value::parse(&self.session.ask("windows")?)?;
        let text = |window: &Value, key| {
            let text = window.get(key).and_then(Value::as_str);
            text.unwrap_or_default().to_string()
        };
        let pair = |window: &Value| (text(window, "title"), text(window, "app_id"));
        Ok(windows.as_array().iter().map(pair).collect())
    }
}

/// What a wait remembers from its beginning.
struct Start {
    status: Status,
    part: Option<Vec<[u8; 3]>>,
    image: Option<Picture>,
}

/// Whether `condition` holds, and what to answer if so.
fn check(
    condition: &Condition,
    watcher: &mut Watcher,
    status: &Status,
    start: &Start,
) -> Result<Option<String>, String> {
    let seen = |met: bool, answer: String| Ok(met.then_some(answer));
    match condition {
        Condition::Ready => seen(status.windows > 0, status.title.clone()),
        Condition::Window(None) => seen(status.windows > 0, status.title.clone()),
        Condition::Window(Some(pattern)) => {
            if status.windows == 0 {
                return Ok(None);
            }
            let windows = watcher.windows()?;
            let found = windows
                .into_iter()
                .find(|(title, app)| glob(pattern, title) || glob(pattern, app));
            Ok(found.map(|(title, _)| title))
        }
        Condition::Title(pattern) => seen(
            status.windows > 0 && glob(pattern, &status.title),
            status.title.clone(),
        ),
        Condition::Closed => seen(status.windows == 0, String::new()),
        Condition::Exited => Ok(status.exited.map(|code| code.to_string())),
        Condition::Stable(time) => seen(
            status.windows > 0 && status.damage > 0 && status.quiet >= *time,
            String::new(),
        ),
        Condition::Changed(None) => seen(status.damage != start.status.damage, String::new()),
        Condition::Changed(Some(region)) => {
            if status.damage == start.status.damage {
                return Ok(None);
            }
            let part = watcher.picture(status)?.part(*region);
            seen(Some(&part) != start.part.as_ref(), String::new())
        }
        Condition::Pixel {
            x,
            y,
            color,
            tolerance,
        } => {
            let pixel = watcher.picture(status)?.pixel(*x, *y);
            let pixel = pixel.ok_or("the pixel is outside the window")?;
            let near = pixel
                .iter()
                .zip(color)
                .all(|(a, b)| a.abs_diff(*b) <= *tolerance);
            seen(near, color_name(pixel))
        }
        Condition::Image { similarity, .. } => {
            let small = start.image.as_ref().expect("loaded at the start");
            let found = watcher.picture(status)?.find(small, *similarity);
            Ok(found.map(|(x, y)| format!("{} {}", x + small.width / 2, y + small.height / 2)))
        }
        Condition::Text(pattern) => {
            let lines = watcher.lines(status)?;
            let found = lines.iter().find(|line| glob(pattern, &line.text));
            Ok(found.map(|line| format!("{} {} {}", line.middle().0, line.middle().1, line.text)))
        }
        Condition::Loaded => {
            // Asking takes a moment, so only when the picture has come to rest a little.
            if status.windows == 0 || status.quiet < Duration::from_millis(150) {
                return Ok(None);
            }
            if let Some(loaded) = watcher.documents_loaded() {
                return seen(loaded, "accessibility".into());
            }
            let busy = ["wait", "progress"].contains(&status.cursor.as_str());
            seen(status.quiet >= LOADED_QUIET && !busy, "picture".into())
        }
        Condition::Cursor(name) => seen(status.cursor == *name, status.cursor.clone()),
        Condition::Clipboard => seen(status.clipboard != start.status.clipboard, String::new()),
        Condition::File(path) => seen(path.exists(), path.display().to_string()),
    }
}

/// Waits until `condition` holds for the invisible window `name`, at most `timeout` (once is
/// always looked). `tick` runs between two looks; an error from it ends the wait. Returns what
/// the condition answers, which may be nothing.
pub fn wait(
    name: &str,
    condition: &Condition,
    timeout: Duration,
    tick: &mut dyn FnMut() -> Result<(), String>,
) -> Result<String, WaitError> {
    wait_for(Watcher::of(name), condition, timeout, tick)
}

/// `wait` for the shadow on a Wayland display.
pub fn wait_on(
    display: &str,
    condition: &Condition,
    timeout: Duration,
    tick: &mut dyn FnMut() -> Result<(), String>,
) -> Result<String, WaitError> {
    wait_for(Watcher::on(display), condition, timeout, tick)
}

fn wait_for(
    watcher: Result<Watcher, String>,
    condition: &Condition,
    timeout: Duration,
    tick: &mut dyn FnMut() -> Result<(), String>,
) -> Result<String, WaitError> {
    let began = Instant::now();
    // A window that is gone has closed and its program has ended.
    let gone = |err: String| match condition.met_when_gone() {
        true => Ok(String::new()),
        false => Err(WaitError::Failed(err)),
    };
    let mut watcher = match watcher {
        Ok(watcher) => watcher,
        Err(err) => return gone(err),
    };
    let status = match watcher.status() {
        Ok(status) => status,
        Err(err) => return gone(err),
    };
    let mut start = Start {
        status: status.clone(),
        part: None,
        image: None,
    };
    match condition {
        Condition::Changed(Some(region)) => {
            let picture = watcher.picture(&status).map_err(WaitError::Failed)?;
            start.part = Some(picture.part(*region));
        }
        Condition::Image { path, .. } => {
            start.image = Some(Picture::load(path).map_err(WaitError::Failed)?);
        }
        _ => {}
    }
    let mut status = status;
    loop {
        match check(condition, &mut watcher, &status, &start) {
            Ok(Some(answer)) => return Ok(answer),
            Ok(None) => {}
            Err(err) => return Err(WaitError::Failed(err)),
        }
        if began.elapsed() >= timeout {
            return Err(WaitError::Timeout(status.describe()));
        }
        tick().map_err(WaitError::Failed)?;
        std::thread::sleep(POLL);
        status = match watcher.status() {
            Ok(status) => status,
            Err(err) => return gone(err),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob("Example", "An Example Domain"));
        assert!(!glob("example", "An Example Domain"));
        assert!(glob("*Domain", "Example Domain"));
        assert!(!glob("*Domain", "Example Domains"));
        assert!(glob("Ex*Do*n", "Example Domain"));
        assert!(glob("?xample*", "Example Domain"));
        assert!(!glob("?", ""));
        assert!(glob("*", ""));
        assert!(glob("a*b*c", "aXbXbXc"));
        assert!(!glob("a*b*c", "aXbXbX"));
        assert!(glob("", "anything"));
        assert!(glob("ä*ö", "äxyö"));
    }

    #[test]
    fn status_lines() {
        let status = Status::parse("status 12 340 2 running text 3 My Title here").unwrap();
        assert_eq!(status.damage, 12);
        assert_eq!(status.quiet, Duration::from_millis(340));
        assert_eq!(status.windows, 2);
        assert_eq!(status.exited, None);
        assert_eq!(status.cursor, "text");
        assert_eq!(status.clipboard, 3);
        assert_eq!(status.title, "My Title here");
        let status = Status::parse("0 0 0 exited:127 default 0 ").unwrap();
        assert_eq!(status.exited, Some(127));
        assert_eq!(status.title, "");
        assert!(Status::parse("status 1 2").is_err());
        assert!(Status::parse("status a 0 0 running default 0 x").is_err());
        assert!(Status::parse("status 1 0 0 stopped default 0 x").is_err());
    }

    #[test]
    fn conditions_parse() {
        let parse = |text: &str| Condition::parse(&text.split(' ').collect::<Vec<_>>());
        assert_eq!(parse("ready"), Ok(Condition::Ready));
        assert_eq!(parse("window"), Ok(Condition::Window(None)));
        assert_eq!(
            parse("window My App"),
            Ok(Condition::Window(Some("My App".into())))
        );
        assert_eq!(parse("title *Done*"), Ok(Condition::Title("*Done*".into())));
        assert_eq!(
            parse("stable"),
            Ok(Condition::Stable(Duration::from_millis(500)))
        );
        assert_eq!(
            parse("stable 80"),
            Ok(Condition::Stable(Duration::from_millis(80)))
        );
        assert_eq!(
            parse("changed 1,2,3,4"),
            Ok(Condition::Changed(Some((1, 2, 3, 4))))
        );
        assert_eq!(
            parse("pixel 3 4 #ff8000"),
            Ok(Condition::Pixel {
                x: 3,
                y: 4,
                color: [255, 128, 0],
                tolerance: 8
            })
        );
        assert_eq!(
            parse("image a.png 0.8"),
            Ok(Condition::Image {
                path: "a.png".into(),
                similarity: 0.8
            })
        );
        assert_eq!(
            parse("file /tmp/a b"),
            Ok(Condition::File("/tmp/a b".into()))
        );
        for bad in [
            "",
            "title",
            "stable soon",
            "pixel 1 2",
            "pixel 1 2 red",
            "image a.png 2",
            "dance",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_color("00ff7f"), Ok([0, 255, 127]));
        assert!(parse_color("#fff").is_err());
        assert_eq!(color_name([0, 255, 127]), "#00ff7f");
    }

    fn picture(width: i32, height: i32, color: impl Fn(i32, i32) -> [u8; 3]) -> Picture {
        let pixels = (0..height).flat_map(|y| (0..width).map(move |x| (x, y)));
        Picture {
            rgb: pixels.flat_map(|(x, y)| color(x, y)).collect(),
            width,
            height,
        }
    }

    #[test]
    fn a_small_picture_is_found_in_a_large_one() {
        let pattern = |x: i32, y: i32| {
            [
                (x * 31 % 256) as u8,
                (y * 17 % 256) as u8,
                ((x + y) * 7 % 256) as u8,
            ]
        };
        let small = picture(12, 9, pattern);
        let large = picture(80, 60, |x, y| match (x - 40, y - 25) {
            (sx @ 0..12, sy @ 0..9) => pattern(sx, sy),
            _ => [200, 200, 200],
        });
        assert_eq!(large.find(&small, 1.0), Some((40, 25)));
        assert_eq!(large.find(&small, 0.9), Some((40, 25)));
        assert_eq!(small.find(&large, 0.9), None);
        let plain = picture(80, 60, |_, _| [200, 200, 200]);
        assert_eq!(plain.find(&small, 0.95), None);

        // A few pixels off still count with a lower similarity, not with a full one.
        let mut scratched = large.clone();
        for x in 40..44 {
            let at = (25 * 80 + x) as usize * 3;
            scratched.rgb[at..at + 3].copy_from_slice(&[0, 0, 0]);
        }
        assert_eq!(scratched.find(&small, 1.0), None);
        assert_eq!(scratched.find(&small, 0.9), Some((40, 25)));

        assert_eq!(large.pixel(40, 25), Some(pattern(0, 0)));
        assert_eq!(large.pixel(80, 0), None);
        assert_eq!(large.part((79, 59, 5, 5)), [[200, 200, 200]]);
    }
}
