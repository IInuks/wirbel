//! Reading the text an invisible window shows, in two ways that work with any program.
//!
//! - By the clipboard: select all and copy are pressed in the window, and its own clipboard is
//!   read. Exact, but it changes the selection in the program, and gives the whole text only.
//! - By text recognition: the helper program `tesseract` reads a picture of the window,
//!   enlarged three times first. It works on everything that draws and says where each line
//!   is, but it takes a moment and may misread. Without `tesseract` installed this way says so and the other still works.

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
     (the package is called tesseract or tesseract-ocr, plus a language such as tesseract-data-eng \
     or tesseract-ocr-eng)";

/// The names the recognition program goes by. Void calls it `tesseract-ocr`, because its
/// `tesseract` is a game; that name comes first so that the game is never started instead.
const TESSERACT_NAMES: [&str; 2] = ["tesseract-ocr", "tesseract"];

/// The recognition program on `PATH`, if there is one.
pub fn tesseract_program() -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    TESSERACT_NAMES.iter().find_map(|name| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|file| file.is_file())
    })
}

/// Reads the text in a picture. `language` is what `tesseract -l` takes, such as `eng+deu`.
pub fn recognise(picture: &Path, language: Option<&str>) -> Result<Vec<Line>, String> {
    let program = tesseract_program().ok_or(NO_TESSERACT)?;
    // Screen text is small for the recognition, which wants letters some 30 pixels high: an
    // enlarged copy reads @, umlauts and thin letters far better (measured on a terminal).
    let (rgb, width, height) = super::shot::read_png(picture)?;
    let factor = scale_factor(width, height);
    let mut enlarged = picture.as_os_str().to_owned();
    enlarged.push(".big.png");
    let enlarged = std::path::PathBuf::from(enlarged);
    let read_from = if factor > 1 {
        let (big, w, h) = enlarge(&rgb, width as usize, height as usize, factor);
        write_rgb_png(&enlarged, &big, w as u32, h as u32)?;
        enlarged.as_path()
    } else {
        picture
    };
    let lines = run_tesseract(&program, read_from, language);
    if factor > 1 {
        let _ = std::fs::remove_file(&enlarged);
    }
    let f = factor as i32;
    Ok(lines?
        .into_iter()
        .map(|line| Line {
            x: line.x / f,
            y: line.y / f,
            w: (line.w + f - 1) / f,
            h: (line.h + f - 1) / f,
            ..line
        })
        .collect())
}

/// How much a picture of this size is enlarged before it is read: three times, less for big
/// windows so that the copy stays below about 12 million pixels. `WIRBEL_OCR_SCALE` sets it.
pub fn scale_factor(width: u32, height: u32) -> usize {
    if let Some(factor) = std::env::var("WIRBEL_OCR_SCALE")
        .ok()
        .and_then(|text| text.parse::<usize>().ok())
    {
        return factor.clamp(1, 4);
    }
    let pixels = (width as f64 * height as f64).max(1.0);
    ((12e6 / pixels).sqrt().floor() as usize).clamp(1, 3)
}

/// The Lanczos kernel with three lobes.
fn lanczos3(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    if x.abs() >= 3.0 {
        return 0.0;
    }
    let pi_x = std::f64::consts::PI * x;
    3.0 * pi_x.sin() * (pi_x / 3.0).sin() / (pi_x * pi_x)
}

/// For each of the `factor` places an output pixel can have between two input pixels: the
/// first input pixel it draws on and the weights of the six it draws on.
fn lanczos_taps(factor: usize) -> Vec<(isize, [f64; 6])> {
    (0..factor)
        .map(|phase| {
            let centre = (phase as f64 + 0.5) / factor as f64 - 0.5;
            let first = centre.floor() as isize - 2;
            let mut weights = [0.0; 6];
            for (k, weight) in weights.iter_mut().enumerate() {
                *weight = lanczos3(centre - (first + k as isize) as f64);
            }
            let sum: f64 = weights.iter().sum();
            weights.iter_mut().for_each(|weight| *weight /= sum);
            (first, weights)
        })
        .collect()
}

/// Enlarges rows of RGB pixels by a whole factor, first along the rows, then down the columns.
pub fn enlarge(rgb: &[u8], width: usize, height: usize, factor: usize) -> (Vec<u8>, usize, usize) {
    let taps = lanczos_taps(factor);
    let (wide, high) = (width * factor, height * factor);
    let mut rows = vec![0f32; wide * height * 3];
    for y in 0..height {
        for x in 0..wide {
            let (first, weights) = &taps[x % factor];
            let base = (x / factor) as isize + first;
            for c in 0..3 {
                let mut sum = 0.0;
                for (k, weight) in weights.iter().enumerate() {
                    let sx = (base + k as isize).clamp(0, width as isize - 1) as usize;
                    sum += weight * rgb[(y * width + sx) * 3 + c] as f64;
                }
                rows[(y * wide + x) * 3 + c] = sum as f32;
            }
        }
    }
    let mut out = vec![0u8; wide * high * 3];
    for y in 0..high {
        let (first, weights) = &taps[y % factor];
        let base = (y / factor) as isize + first;
        for x in 0..wide {
            for c in 0..3 {
                let mut sum = 0.0;
                for (k, weight) in weights.iter().enumerate() {
                    let sy = (base + k as isize).clamp(0, height as isize - 1) as usize;
                    sum += weight * rows[(sy * wide + x) * 3 + c] as f64;
                }
                out[(y * wide + x) * 3 + c] = sum.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    (out, wide, high)
}

fn write_rgb_png(path: &Path, rgb: &[u8], width: u32, height: u32) -> Result<(), String> {
    let fail = |err: &dyn std::fmt::Display| format!("{}: {err}", path.display());
    let file = std::fs::File::create(path).map_err(|err| fail(&err))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(|err| fail(&err))?;
    writer.write_image_data(rgb).map_err(|err| fail(&err))?;
    writer.finish().map_err(|err| fail(&err))
}

/// All installed languages, English last: its letters are in most other Latin scripts, and
/// put first it reads ö as R. Leaves out the ones that are not a language of text.
fn default_languages(program: &Path) -> Option<String> {
    let output = Command::new(program)
        .arg("--list-langs")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let languages = pick_languages(&String::from_utf8_lossy(&output.stdout));
    (!languages.is_empty()).then_some(languages)
}

fn pick_languages(listing: &str) -> String {
    let mut languages: Vec<&str> = listing
        .lines()
        .skip_while(|line| !line.starts_with("List of"))
        .skip(1)
        .map(str::trim)
        .filter(|name| {
            !name.is_empty()
                && !["osd", "equ", "snum"].contains(name)
                && !name.ends_with("_frak")
                && !name.contains('/')
        })
        .collect();
    languages.sort_by_key(|name| *name == "eng");
    languages.join("+")
}

fn run_tesseract(
    program: &Path,
    picture: &Path,
    language: Option<&str>,
) -> Result<Vec<Line>, String> {
    let mut command = Command::new(program);
    command.arg(picture).arg("stdout");
    let language = language
        .map(str::to_string)
        .or_else(|| std::env::var("WIRBEL_OCR_LANG").ok())
        .or_else(|| default_languages(program));
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

    #[test]
    fn enlarging_keeps_flat_colour_and_the_size() {
        let rgb = [10u8, 200, 30].repeat(4 * 3);
        let (big, w, h) = enlarge(&rgb, 4, 3, 3);
        assert_eq!((w, h), (12, 9));
        assert_eq!(big.len(), 12 * 9 * 3);
        assert!(big.chunks_exact(3).all(|p| p == [10, 200, 30]));
    }

    #[test]
    fn enlarging_keeps_an_edge_sharp() {
        // Black left half, white right half: far from the edge it stays black and white.
        let rgb: Vec<u8> = (0..8 * 2)
            .flat_map(|i| if i % 8 < 4 { [0; 3] } else { [255; 3] })
            .collect();
        let (big, w, _) = enlarge(&rgb, 8, 2, 3);
        assert_eq!(big[0], 0);
        assert_eq!(big[(w - 1) * 3], 255);
    }

    #[test]
    fn english_comes_last_among_the_installed_languages() {
        let listing = "List of available languages in \"/usr/share/tessdata/\" (5):\ndeu\ndeu_frak\neng\nequ\nosd\n";
        assert_eq!(pick_languages(listing), "deu+eng");
        assert_eq!(
            pick_languages("List of available languages (1):\neng\n"),
            "eng"
        );
        assert_eq!(pick_languages(""), "");
    }

    #[test]
    fn big_windows_are_enlarged_less() {
        assert_eq!(scale_factor(1280, 800), 3);
        assert_eq!(scale_factor(2560, 1440), 1);
        assert_eq!(scale_factor(1920, 1080), 2);
        assert_eq!(scale_factor(640, 400), 3);
    }

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
