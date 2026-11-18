//! Pictures of what a shadow shows, written as PNG files.
//!
//! `shadow shot` TAB PATH TAB X,Y,W,H|- TAB cursor|- asks for one. The answer comes after the
//! next frame: `ok WxH`, the size of the picture. A PATH that ends in `.rgba` gets the bare
//! RGBA pixels row by row instead of a PNG, for a viewer that wants many pictures a second. The picture is drawn once more for this, as
//! screen captures are, without the pointer unless asked for.

use std::{
    fs,
    io::BufWriter,
    path::{Path, PathBuf},
};

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, ImportAll, ImportMem, Offscreen, Renderer, Texture,
            damage::OutputDamageTracker,
            element::{AsRenderElements, Element, Kind, surface::WaylandSurfaceRenderElement},
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::Window,
    output::Output,
    utils::{Buffer, Physical, Point, Rectangle, Scale, Size, Transform},
};

use crate::Wirbel;

/// A picture someone waits for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shot {
    /// The connection to answer.
    pub peer: u64,
    pub path: PathBuf,
    /// The part to keep, in pixels of the output. `None` is all of it.
    pub region: Option<(i32, i32, i32, i32)>,
    pub cursor: bool,
}

impl Shot {
    /// The fields after `shadow shot` TAB.
    pub fn parse(peer: u64, rest: &str) -> Result<Self, String> {
        let mut fields = rest.split('\t');
        let path = fields.next().filter(|path| path.starts_with('/'));
        let path = path.ok_or("expected an absolute path for the picture")?;
        let region = match fields.next() {
            None | Some("-") => None,
            Some(text) => Some(parse_region(text)?),
        };
        let cursor = match fields.next() {
            None | Some("-") => false,
            Some("cursor") => true,
            Some(other) => return Err(format!("expected cursor or -, got `{other}`")),
        };
        Ok(Self {
            peer,
            path: path.into(),
            region,
            cursor,
        })
    }
}

/// `X,Y,W,H`.
pub fn parse_region(text: &str) -> Result<(i32, i32, i32, i32), String> {
    let numbers: Vec<i32> = text.split(',').filter_map(|n| n.parse().ok()).collect();
    match numbers[..] {
        [x, y, w, h] if text.split(',').count() == 4 && x >= 0 && y >= 0 && w > 0 && h > 0 => {
            Ok((x, y, w, h))
        }
        _ => Err(format!("expected a region X,Y,W,H, got `{text}`")),
    }
}

/// The part of a picture of `size` that `region` names, cut to the picture.
pub fn crop(
    size: (i32, i32),
    region: Option<(i32, i32, i32, i32)>,
) -> Option<(i32, i32, i32, i32)> {
    let (x, y, w, h) = region.unwrap_or((0, 0, size.0, size.1));
    let (w, h) = (w.min(size.0 - x), h.min(size.1 - y));
    (x < size.0 && y < size.1 && w > 0 && h > 0).then_some((x, y, w, h))
}

/// Writes rows of RGBA pixels as an RGB PNG: what is on a screen has no see-through parts.
pub fn write_png(
    path: &Path,
    pixels: &[u8],
    width: i32,
    (x, y, w, h): (i32, i32, i32, i32),
) -> Result<(), String> {
    let row = width as usize * 4;
    let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
    for line in pixels.chunks_exact(row).skip(y as usize).take(h as usize) {
        for pixel in line.chunks_exact(4).skip(x as usize).take(w as usize) {
            rgb.extend_from_slice(&pixel[..3]);
        }
    }
    if rgb.len() != w as usize * h as usize * 3 {
        return Err("too few pixels came back".into());
    }
    // Next to the target and renamed, so that nobody reads half a picture.
    let mut partial = path.as_os_str().to_owned();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    let written = (|| -> Result<(), Box<dyn std::error::Error>> {
        let file = BufWriter::new(fs::File::create(&partial)?);
        let mut encoder = png::Encoder::new(file, w as u32, h as u32);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&rgb)?;
        writer.finish()?;
        fs::rename(&partial, path)?;
        Ok(())
    })();
    written.map_err(|err| {
        let _ = fs::remove_file(&partial);
        format!("{}: {err}", path.display())
    })
}

/// Writes the picture as a PNG, or as bare RGBA rows if the path ends in `.rgba`.
pub fn write_picture(
    path: &Path,
    pixels: &[u8],
    width: i32,
    part: (i32, i32, i32, i32),
) -> Result<(), String> {
    if path.extension().is_none_or(|extension| extension != "rgba") {
        return write_png(path, pixels, width, part);
    }
    let (x, y, w, h) = part;
    let row = width as usize * 4;
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for line in pixels.chunks_exact(row).skip(y as usize).take(h as usize) {
        rgba.extend_from_slice(&line[x as usize * 4..(x + w) as usize * 4]);
    }
    if rgba.len() != w as usize * h as usize * 4 {
        return Err("too few pixels came back".into());
    }
    let mut partial = path.as_os_str().to_owned();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    fs::write(&partial, &rgba)
        .and_then(|()| fs::rename(&partial, path))
        .map_err(|err| {
            let _ = fs::remove_file(&partial);
            format!("{}: {err}", path.display())
        })
}

/// Reads a PNG back as rows of RGB pixels with its width and height.
pub fn read_png(path: &Path) -> Result<(Vec<u8>, u32, u32), String> {
    let fail = |err: &dyn std::fmt::Display| format!("{}: {err}", path.display());
    let file = fs::File::open(path).map_err(|err| fail(&err))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|err| fail(&err))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| fail(&"too large"))?;
    let mut data = vec![0; size];
    let info = reader.next_frame(&mut data).map_err(|err| fail(&err))?;
    data.truncate(info.buffer_size());
    let rgb: Vec<u8> = match info.color_type {
        png::ColorType::Rgb => data,
        png::ColorType::Rgba => data
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|g| [*g, *g, *g]).collect(),
        png::ColorType::GrayscaleAlpha => data
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0]])
            .collect(),
        png::ColorType::Indexed => return Err(fail(&"unexpected palette")),
    };
    Ok((rgb, info.width, info.height))
}

/// Draws what `output` shows once more and reads it back as rows of RGBA pixels.
fn picture(
    state: &Wirbel,
    renderer: &mut GlesRenderer,
    output: &Output,
    cursor: bool,
) -> Result<(Vec<u8>, (i32, i32)), String> {
    let mode = output.current_mode().ok_or("the output has no mode")?;
    let size = Size::<i32, Buffer>::from((mode.size.w, mode.size.h));
    let fail = |what: &str, err: &dyn std::fmt::Debug| format!("{what}: {err:?}");
    let mut texture: GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, size)
        .map_err(|err| fail("no room for the picture", &err))?;
    let mut elements = state.output_elements(renderer, output);
    if !cursor {
        elements.retain(|element| element.kind() != Kind::Cursor);
    }
    let mut tracker = OutputDamageTracker::from_output(output);
    let mut framebuffer = renderer
        .bind(&mut texture)
        .map_err(|err| fail("failed to bind the picture", &err))?;
    tracker
        .render_output(
            renderer,
            &mut framebuffer,
            0,
            &elements,
            state.config.background_color,
        )
        .map_err(|err| fail("failed to draw the picture", &err))?;
    let mapping = renderer
        .copy_framebuffer(&framebuffer, Rectangle::from_size(size), Fourcc::Abgr8888)
        .map_err(|err| fail("failed to read the picture back", &err))?;
    let pixels = renderer
        .map_texture(&mapping)
        .map_err(|err| fail("failed to map the picture", &err))?;
    Ok((pixels.to_vec(), (mode.size.w, mode.size.h)))
}

/// Draws a window alone, as large as it is, and reads it back as rows of RGBA pixels.
fn window_picture<R>(renderer: &mut R, window: &Window) -> Result<(Vec<u8>, (i32, i32)), String>
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesTexture> + Bind<GlesTexture>,
    R::TextureId: Clone + Texture + 'static,
{
    let geometry = window.geometry();
    let (w, h) = (geometry.size.w, geometry.size.h);
    if w < 1 || h < 1 {
        return Err("the window has no size yet".into());
    }
    let fail = |what: &str, err: &dyn std::fmt::Debug| format!("{what}: {err:?}");
    let mut texture: GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, Size::<i32, Buffer>::from((w, h)))
        .map_err(|err| fail("no room for the picture", &err))?;
    // What the window calls its content begins at the origin; shadows around it fall outside.
    let origin: Point<i32, Physical> = (-geometry.loc.x, -geometry.loc.y).into();
    let elements: Vec<WaylandSurfaceRenderElement<R>> =
        window.render_elements(renderer, origin, Scale::from(1.0), 1.0);
    let mut tracker = OutputDamageTracker::new((w, h), 1.0, Transform::Normal);
    let mut framebuffer = renderer
        .bind(&mut texture)
        .map_err(|err| fail("failed to bind the picture", &err))?;
    tracker
        .render_output(
            renderer,
            &mut framebuffer,
            0,
            &elements,
            [0.0, 0.0, 0.0, 1.0],
        )
        .map_err(|err| fail("failed to draw the picture", &err))?;
    let region = Rectangle::from_size(Size::<i32, Buffer>::from((w, h)));
    let mapping = renderer
        .copy_framebuffer(&framebuffer, region, Fourcc::Abgr8888)
        .map_err(|err| fail("failed to read the picture back", &err))?;
    let pixels = renderer
        .map_texture(&mapping)
        .map_err(|err| fail("failed to map the picture", &err))?;
    Ok((pixels.to_vec(), (w, h)))
}

/// Answers the pictures of parked windows that wait. Called by every backend after a frame.
pub fn take_parked<R>(state: &mut Wirbel, renderer: &mut R)
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesTexture> + Bind<GlesTexture>,
    R::TextureId: Clone + Texture + 'static,
{
    for (shot, window) in std::mem::take(&mut state.vwm.parked_shots) {
        let answer = window_picture(renderer, &window).and_then(|(pixels, size)| {
            let part = crop(size, shot.region).ok_or("the region is outside the picture")?;
            write_picture(&shot.path, &pixels, size.0, part)?;
            Ok(format!("ok {}x{}", part.2, part.3))
        });
        let answer = answer.unwrap_or_else(|err: String| format!("error {err}"));
        state.ipc.reply(shot.peer, &answer);
    }
}

/// Answers the pictures that wait. Called by the backend after a frame of `output`.
pub fn take(state: &mut Wirbel, renderer: &mut GlesRenderer, output: &Output) {
    if state.vwm.shots.is_empty() {
        return;
    }
    let shots = std::mem::take(&mut state.vwm.shots);
    for cursor in [false, true] {
        let due: Vec<&Shot> = shots.iter().filter(|shot| shot.cursor == cursor).collect();
        if due.is_empty() {
            continue;
        }
        let picture = picture(state, renderer, output, cursor);
        for shot in due {
            let answer = picture
                .as_ref()
                .map_err(String::clone)
                .and_then(|(pixels, size)| {
                    let part =
                        crop(*size, shot.region).ok_or("the region is outside the picture")?;
                    write_picture(&shot.path, pixels, size.0, part)?;
                    Ok(format!("ok {}x{}", part.2, part.3))
                });
            let answer = answer.unwrap_or_else(|err: String| format!("error {err}"));
            state.ipc.reply(shot.peer, &answer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions() {
        assert_eq!(parse_region("1,2,30,40"), Ok((1, 2, 30, 40)));
        assert!(parse_region("1,2,30").is_err());
        assert!(parse_region("1,2,0,40").is_err());
        assert!(parse_region("-1,2,3,4").is_err());
        assert!(parse_region("1,2,3,4,5").is_err());
        assert!(parse_region("a,b,c,d").is_err());
        assert_eq!(crop((100, 50), None), Some((0, 0, 100, 50)));
        assert_eq!(
            crop((100, 50), Some((90, 40, 30, 30))),
            Some((90, 40, 10, 10))
        );
        assert_eq!(crop((100, 50), Some((100, 0, 5, 5))), None);
    }

    #[test]
    fn shots_parse() {
        let shot = Shot::parse(7, "/tmp/a b.png\t1,2,3,4\tcursor").unwrap();
        assert_eq!(shot.path, PathBuf::from("/tmp/a b.png"));
        assert_eq!(shot.region, Some((1, 2, 3, 4)));
        assert!(shot.cursor && shot.peer == 7);
        let shot = Shot::parse(1, "/x.png").unwrap();
        assert!(shot.region.is_none() && !shot.cursor);
        assert!(Shot::parse(1, "relative.png").is_err());
        assert!(Shot::parse(1, "/x.png\t-\tpointer").is_err());
    }

    #[test]
    fn bare_pixels_are_cut_like_a_png() {
        let path = std::env::temp_dir().join(format!("wirbel-raw-{}.rgba", std::process::id()));
        let pixels: Vec<u8> = (0..6u8).flat_map(|n| [n, n, n, 255]).collect();
        write_picture(&path, &pixels, 3, (1, 0, 2, 2)).unwrap();
        let raw = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let expected: Vec<u8> = [1u8, 2, 4, 5]
            .into_iter()
            .flat_map(|n| [n, n, n, 255])
            .collect();
        assert_eq!(raw, expected);
    }

    #[test]
    fn a_png_comes_back_as_it_went() {
        let path = std::env::temp_dir().join(format!("wirbel-png-{}.png", std::process::id()));
        // Three by two pixels, RGBA, each its own colour.
        let pixels: Vec<u8> = (0..6u8)
            .flat_map(|n| [n * 40, 255 - n * 40, n, 255])
            .collect();
        write_png(&path, &pixels, 3, (1, 0, 2, 2)).unwrap();
        let (rgb, width, height) = read_png(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!((width, height), (2, 2));
        let pixel = |n: u8| [n * 40, 255 - n * 40, n];
        let expected: Vec<u8> = [1, 2, 4, 5].into_iter().flat_map(pixel).collect();
        assert_eq!(rgb, expected);
    }
}
