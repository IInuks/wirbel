//! Reading the text an invisible window shows, in two ways that work with any program.
//!
//! - By the clipboard: select all and copy are pressed in the window, and its own clipboard is
//!   read. Exact, but it changes the selection in the program, and gives the whole text only.
//! - By text recognition: the helper program `tesseract` reads a picture of the window. It
//!   works on everything that draws and says where each line is, but it takes a moment and
//!   may misread. Without `tesseract` installed this way says so and the other still works.

use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use super::{
    cli::{Session, clip_get_on, key_request},
    wait::{Status, glob},
};

/// A line of text and the box around it, in pixels of the window.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Line {
    pub fn middle(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// The lines in what `tesseract ... tsv` prints: its words, put together by the line they are
/// in, with the box around them. Words it is not sure are words at all are left out.
pub fn parse_tsv(tsv: &str) -> Vec<Line> {
    let mut lines: Vec<((u32, u32, u32, u32), Line)> = Vec::new();
    for row in tsv.lines().skip(1) {
        let fields: Vec<&str> = row.splitn(12, '\t').collect();
        let [
            level,
            page,
            block,
            paragraph,
            line,
            _,
            left,
            top,
            width,
            height,
            confidence,
            text,
        ] = fields[..]
        else {
            continue;
        };
        let number = |text: &str| text.parse::<i32>().ok();
        let key = |text: &str| text.parse::<u32>().ok();
        let (Some(x), Some(y), Some(w), Some(h)) =
            (number(left), number(top), number(width), number(height))
        else {
            continue;
        };
        let (Some(page), Some(block), Some(paragraph), Some(line)) =
            (key(page), key(block), key(paragraph), key(line))
        else {
            continue;
        };
        let sure = confidence.parse::<f32>().is_ok_and(|c| c >= 0.0);
        let text = text.trim();
        if level != "5" || !sure || text.is_empty() {
            continue;
        }
        let key = (page, block, paragraph, line);
        match lines.iter_mut().find(|(other, _)| *other == key) {
            Some((_, whole)) => {
                let (right, bottom) = (
                    (whole.x + whole.w).max(x + w),
                    (whole.y + whole.h).max(y + h),
                );
                whole.x = whole.x.min(x);
                whole.y = whole.y.min(y);
                (whole.w, whole.h) = (right - whole.x, bottom - whole.y);
                whole.text.push(' ');
                whole.text.push_str(text);
            }
            None => lines.push((
                key,
                Line {
                    text: text.to_string(),
                    x,
                    y,
                    w,
                    h,
                },
            )),
        }
    }
    lines.into_iter().map(|(_, line)| line).collect()
}

pub const NO_TESSERACT: &str = "text recognition needs the program `tesseract`, which is not installed \
     (the package is called tesseract or tesseract-ocr, plus a language such as tesseract-data-eng)";

/// Reads the text in a picture. `language` is what `tesseract -l` takes, such as `eng+deu`.
pub fn recognise(picture: &Path, language: Option<&str>) -> Result<Vec<Line>, String> {
    let mut command = Command::new("tesseract");
    command.arg(picture).arg("stdout");
    let language = language
        .map(str::to_string)
        .or_else(|| std::env::var("WIRBEL_OCR_LANG").ok());
    if let Some(language) = language.filter(|language| !language.is_empty()) {
        command.args(["-l", &language]);
    }
    let output = command
        .arg("tsv")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let output = match output {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(NO_TESSERACT.into()),
        Err(err) => return Err(format!("failed to run tesseract: {err}")),
    };
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tesseract failed: {}", why.trim()));
    }
    Ok(parse_tsv(&String::from_utf8_lossy(&output.stdout)))
}

/// The lines that match a pattern, all of them if there is none.
pub fn matching<'a>(lines: &'a [Line], pattern: Option<&str>) -> Vec<&'a Line> {
    let fits = |line: &&Line| pattern.is_none_or(|pattern| glob(pattern, &line.text));
    lines.iter().filter(fits).collect()
}

/// How long a program may take to copy after the keys.
const COPY_PATIENCE: Duration = Duration::from_secs(2);

/// Reads the text of the window on this display through its clipboard: presses `keys` (select
/// all, then copy) and waits for something new in the clipboard.
pub fn by_clipboard(display: &str, keys: &[&str]) -> Result<String, String> {
    let mut session = Session::on(display)?;
    let before = Status::parse(&session.ask("shadow status")?)?.clipboard;
    for key in keys {
        session.ask(&key_request(key))?;
        std::thread::sleep(Duration::from_millis(60));
    }
    let start = Instant::now();
    loop {
        let now = Status::parse(&session.ask("shadow status")?)?.clipboard;
        if now != before {
            // The selection is announced before the program is ready to hand it over.
            return clip_get_on(display);
        }
        if start.elapsed() > COPY_PATIENCE {
            return Err("the program copied nothing after select all and copy".into());
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TSV: &str = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext
1\t1\t0\t0\t0\t0\t0\t0\t640\t400\t-1\t
4\t1\t1\t1\t1\t0\t10\t20\t200\t18\t-1\t
5\t1\t1\t1\t1\t1\t10\t20\t80\t18\t96.5\tExample
5\t1\t1\t1\t1\t2\t100\t22\t110\t16\t91.0\tDomain
5\t1\t1\t1\t2\t1\t10\t60\t40\t14\t88\tMore
5\t1\t1\t1\t2\t2\t60\t60\t5\t14\t-1\t|
5\t1\t1\t1\t2\t3\t70\t58\t90\t18\t80\tinformation...
5\t1\t2\t1\t1\t1\t300\t300\t50\t20\t70\t 
broken line";

    #[test]
    fn words_become_lines_with_their_box() {
        let lines = parse_tsv(TSV);
        assert_eq!(
            lines,
            [
                Line {
                    text: "Example Domain".into(),
                    x: 10,
                    y: 20,
                    w: 200,
                    h: 18
                },
                Line {
                    text: "More information...".into(),
                    x: 10,
                    y: 58,
                    w: 150,
                    h: 18
                },
            ]
        );
        assert_eq!(lines[0].middle(), (110, 29));
        assert_eq!(matching(&lines, None).len(), 2);
        assert_eq!(matching(&lines, Some("More*")), [&lines[1]]);
        assert_eq!(matching(&lines, Some("Domain")), [&lines[0]]);
        assert!(matching(&lines, Some("nothing")).is_empty());
        assert!(parse_tsv("").is_empty());
    }
}
