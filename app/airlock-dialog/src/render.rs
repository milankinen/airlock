//! Software drawing.
//!
//! Draws filled boxes and text into a block of pixels in memory. Text uses
//! one embedded monospace font on a fixed grid of cells, so that columns
//! line up and the width of a line is its number of characters.

use std::collections::HashMap;

/// Pixel as `0x00RRGGBB`.
pub(crate) type Color = u32;

/// The embedded font.
const FONT: &[u8] = include_bytes!("../assets/DejaVuSansMono.ttf");

/// Box on the canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    /// Make a box.
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    /// Return `true` if the point is in the box.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        (self.x..self.x + self.w).contains(&x) && (self.y..self.y + self.h).contains(&y)
    }
}

/// Block of pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Canvas {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Pixels, row by row.
    pub pixels: Vec<Color>,
}

impl Canvas {
    /// Make a canvas filled with one color.
    pub fn new(width: usize, height: usize, color: Color) -> Self {
        Self {
            width,
            height,
            pixels: vec![color; width * height],
        }
    }

    /// Fill a box. The part outside of the canvas is cut.
    pub fn fill(&mut self, rect: Rect, color: Color) {
        let x0 = (rect.x.max(0) as usize).min(self.width);
        let y0 = (rect.y.max(0) as usize).min(self.height);
        let x1 = ((rect.x + rect.w).max(0) as usize).clamp(x0, self.width);
        let y1 = ((rect.y + rect.h).max(0) as usize).min(self.height);
        for row in y0..y1 {
            self.pixels[row * self.width + x0..row * self.width + x1].fill(color);
        }
    }

    /// Draw the border of a box, `t` pixels thick.
    pub fn border(&mut self, r: Rect, thick: i32, color: Color) {
        self.fill(Rect::new(r.x, r.y, r.w, thick), color);
        self.fill(Rect::new(r.x, r.y + r.h - thick, r.w, thick), color);
        self.fill(Rect::new(r.x, r.y, thick, r.h), color);
        self.fill(Rect::new(r.x + r.w - thick, r.y, thick, r.h), color);
    }

    /// Mix one pixel with a color. `alpha` is the cover, 0 to 255.
    fn blend(&mut self, x: i32, y: i32, color: Color, alpha: u8) {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height || alpha == 0 {
            return;
        }
        let px = &mut self.pixels[y as usize * self.width + x as usize];
        let a = u32::from(alpha);
        let mix = |shift: u32| {
            let bg = (*px >> shift) & 0xff;
            let fg = (color >> shift) & 0xff;
            ((bg * (255 - a) + fg * a) / 255) << shift
        };
        *px = mix(16) | mix(8) | mix(0);
    }

    /// Get the pixels as bytes for an X11 image of depth 24 or 32, with
    /// little-endian pixels.
    pub fn to_bytes(&self, rows: std::ops::Range<usize>) -> Vec<u8> {
        self.pixels[rows.start * self.width..rows.end * self.width]
            .iter()
            .flat_map(|p| p.to_le_bytes())
            .collect()
    }
}

/// Font at one size, with a cache of drawn characters.
pub(crate) struct Font {
    face: fontdue::Font,
    size: f32,
    cache: HashMap<char, (fontdue::Metrics, Vec<u8>)>,
    /// Width of one cell in pixels.
    pub cell_w: i32,
    /// Height of one line in pixels.
    pub cell_h: i32,
    /// Distance from the top of a line to the baseline.
    ascent: i32,
}

impl Font {
    /// Load the embedded font.
    /// Args:
    ///  - `size`: Size in pixels
    pub fn new(size: f32) -> Self {
        let face = fontdue::Font::from_bytes(FONT, fontdue::FontSettings::default())
            .expect("the embedded font is valid");
        let line = face.horizontal_line_metrics(size);
        let ascent = line.map_or(size, |l| l.ascent);
        let height = line.map_or(size * 1.2, |l| l.new_line_size);
        Self {
            cell_w: face.metrics('M', size).advance_width.round() as i32,
            cell_h: height.ceil() as i32 + 2,
            ascent: ascent.round() as i32 + 1,
            face,
            size,
            cache: HashMap::new(),
        }
    }

    /// Draw text on one line. Character `n` has its cell at `x + n * cell_w`.
    /// Args:
    ///  - `x`, `y`: Top left corner of the first cell
    pub fn draw(&mut self, canvas: &mut Canvas, x: i32, y: i32, text: &str, color: Color) {
        for (n, c) in text.chars().enumerate() {
            let (metrics, bitmap) = self
                .cache
                .entry(c)
                .or_insert_with(|| self.face.rasterize(c, self.size));
            let gx = x + n as i32 * self.cell_w + metrics.xmin;
            let gy = y + self.ascent - metrics.height as i32 - metrics.ymin;
            for row in 0..metrics.height {
                for col in 0..metrics.width {
                    canvas.blend(
                        gx + col as i32,
                        gy + row as i32,
                        color,
                        bitmap[row * metrics.width + col],
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that text changes pixels only inside its cells.
    ///   1. Draw two characters on a white canvas
    ///   2. Check that some pixels changed in the cells
    ///   3. Check that no pixel changed outside of the cells
    #[test]
    fn text_draws_inside_its_cells() {
        let mut font = Font::new(16.0);
        let mut canvas = Canvas::new(100, 40, 0xffffff);
        font.draw(&mut canvas, 10, 5, "Hi", 0x000000);
        let changed: Vec<(usize, usize)> = (0..canvas.height)
            .flat_map(|y| (0..canvas.width).map(move |x| (x, y)))
            .filter(|&(x, y)| canvas.pixels[y * canvas.width + x] != 0xffffff)
            .collect();
        assert!(!changed.is_empty());
        let (x0, x1) = (10, 10 + 2 * font.cell_w as usize);
        let (y0, y1) = (5, 5 + font.cell_h as usize);
        assert!(
            changed
                .iter()
                .all(|&(x, y)| (x0..x1).contains(&x) && (y0..y1).contains(&y))
        );
    }

    /// Test that drawing outside of the canvas is cut and does not panic.
    ///   1. Fill and draw partly and fully outside of a small canvas
    ///   2. Check that the call returns
    #[test]
    fn drawing_outside_is_cut() {
        let mut font = Font::new(16.0);
        let mut canvas = Canvas::new(20, 20, 0);
        canvas.fill(Rect::new(-5, -5, 100, 100), 0xff0000);
        canvas.fill(Rect::new(50, 50, 10, 10), 0xff0000);
        // Starts right of the canvas, ends below it.
        canvas.fill(Rect::new(30, 15, 10, 10), 0xff0000);
        font.draw(&mut canvas, -30, 15, "overflow text", 0xffffff);
        assert!(canvas.pixels.contains(&0xff0000));
    }
}
