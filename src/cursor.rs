//! Named cursors for backends without a host that draws them: the Xcursor theme, and a plain arrow
//! if there is none, because a pointer has to be visible. Animated cursors show their first frame.

use std::{cell::RefCell, collections::HashMap, env, fs};

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::{Physical, Point, Transform},
};
use xcursor::{CursorTheme, parser::parse_xcursor};

/// Size of the built-in arrow in pixels, at scale 1.
const ARROW: i32 = 16;

/// A cursor ready to draw, in physical pixels.
#[derive(Debug, Clone)]
pub struct CursorImage {
    pub buffer: MemoryRenderBuffer,
    pub hotspot: Point<i32, Physical>,
}

/// Loads cursors on demand and keeps them.
pub struct Cursors {
    /// The theme asked for and what to fall back on. `default` often is no theme at all, and may
    /// exist in one stratum only.
    themes: [CursorTheme; 2],
    name: String,
    size: u32,
    loaded: RefCell<HashMap<(String, u32), CursorImage>>,
}

impl std::fmt::Debug for Cursors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cursors({}, {})", self.name, self.size)
    }
}

/// A black arrow with a white edge. Pixels are ARGB, little endian.
fn built_in_arrow(pixels_per_unit: i32) -> (Vec<u8>, i32, i32) {
    let side = ARROW * pixels_per_unit;
    let mut pixels = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let inside = x <= y && x + y / 2 < side;
            let edge = inside && (x == 0 || x == y || x + 1 + y / 2 >= side || y + 1 == side);
            pixels.extend_from_slice(match (inside, edge) {
                (false, _) => &[0, 0, 0, 0],
                (true, true) => &[255, 255, 255, 255],
                (true, false) => &[0, 0, 0, 255],
            });
        }
    }
    (pixels, side, side)
}

impl Cursors {
    /// The theme and size are named by the configuration, then by `XCURSOR_THEME` and
    /// `XCURSOR_SIZE`, then it is `default` at 24.
    pub fn load(theme: Option<&str>, size: Option<u32>) -> Self {
        let name = theme
            .map(str::to_string)
            .or_else(|| env::var("XCURSOR_THEME").ok())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "default".to_string());
        let size = size
            .or_else(|| env::var("XCURSOR_SIZE").ok()?.parse().ok())
            .filter(|size| *size > 0)
            .unwrap_or(24);
        Self {
            themes: [CursorTheme::load(&name), CursorTheme::load("Adwaita")],
            name,
            size,
            loaded: RefCell::default(),
        }
    }

    /// One line for `wirbel check`.
    pub fn describe(&self) -> String {
        let found = self
            .themes
            .iter()
            .position(|theme| theme.load_icon("default").is_some());
        let source = match found {
            Some(0) => format!("theme {}", self.name),
            Some(_) => format!("Adwaita instead of the theme {}", self.name),
            None => format!("the built-in arrow, there is no theme {}", self.name),
        };
        format!("cursor from {source}, size {}", self.size)
    }

    fn themed(&self, names: &[&str], pixels: u32) -> Option<CursorImage> {
        let path = self
            .themes
            .iter()
            .find_map(|theme| names.iter().find_map(|name| theme.load_icon(name)))?;
        let data = fs::read(&path).ok()?;
        let images = parse_xcursor(&data)?;
        let distance = |image: &&xcursor::parser::Image| image.size.abs_diff(pixels);
        let image = images.iter().min_by_key(distance)?;
        // Xcursor keeps little endian ARGB, whatever the field of the parser is called.
        let buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Argb8888,
            (image.width as i32, image.height as i32),
            1,
            Transform::Normal,
            None,
        );
        Some(CursorImage {
            buffer,
            hotspot: (image.xhot as i32, image.yhot as i32).into(),
        })
    }

    /// The default arrow for X clients: pixels, size and hotspot. An X cursor is little endian ARGB
    /// like an Xcursor file, whatever Smithay calls the format.
    pub fn default_pixels(&self) -> (Vec<u8>, (u16, u16), (u16, u16)) {
        let image = ["default", "left_ptr"].iter().find_map(|name| {
            let path = self.themes.iter().find_map(|theme| theme.load_icon(name))?;
            let images = parse_xcursor(&fs::read(path).ok()?)?;
            let distance = |image: &&xcursor::parser::Image| image.size.abs_diff(self.size);
            images.iter().min_by_key(distance).cloned()
        });
        match image {
            Some(image) => {
                let (xhot, yhot) = (image.xhot as u16, image.yhot as u16);
                let size = (image.width as u16, image.height as u16);
                (image.pixels_rgba, size, (xhot, yhot))
            }
            None => {
                let (pixels, width, height) = built_in_arrow(1);
                (pixels, (width as u16, height as u16), (0, 0))
            }
        }
    }

    fn built_in(&self, pixels: u32) -> CursorImage {
        let unit = (pixels as i32 / (ARROW + 8)).max(1);
        let (data, width, height) = built_in_arrow(unit);
        let buffer = MemoryRenderBuffer::from_slice(
            &data,
            Fourcc::Argb8888,
            (width, height),
            1,
            Transform::Normal,
            None,
        );
        CursorImage {
            buffer,
            hotspot: (0, 0).into(),
        }
    }

    /// The cursor called `name`, or one of `alternatives`, sized for an output of this `scale`.
    /// An icon the theme lacks shows as its default arrow, or as the built-in one.
    pub fn image(&self, name: &str, alternatives: &[&str], scale: f64) -> CursorImage {
        let pixels = (f64::from(self.size) * scale).round() as u32;
        let key = (name.to_string(), pixels);
        if let Some(image) = self.loaded.borrow().get(&key) {
            return image.clone();
        }
        let mut names = vec![name];
        names.extend_from_slice(alternatives);
        names.push("default");
        names.push("left_ptr");
        let image = self.themed(&names, pixels).unwrap_or_else(|| {
            tracing::warn!(
                theme = self.name,
                name,
                "no such cursor, using the built-in arrow"
            );
            self.built_in(pixels)
        });
        self.loaded.borrow_mut().insert(key, image.clone());
        image
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_arrow_has_a_visible_tip_and_transparent_corner() {
        let (pixels, width, height) = built_in_arrow(1);
        assert_eq!(pixels.len(), (width * height * 4) as usize);
        let at = |x: i32, y: i32| pixels[((y * width + x) * 4 + 3) as usize];
        assert_eq!(at(0, 0), 255);
        assert_eq!(at(width - 1, 0), 0);
    }
}
