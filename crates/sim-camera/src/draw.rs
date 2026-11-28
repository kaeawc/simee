//! Drawing for the synthetic sources: a BGRA canvas, filled rectangles, colours by name or hex,
//! and text in the public-domain 8x8 bitmap font.

use crate::error::{Error, Result};

/// An opaque colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colour {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Colour {
    pub const BLACK: Colour = Colour::rgb(0, 0, 0);
    pub const WHITE: Colour = Colour::rgb(255, 255, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Colour {
        Colour { r, g, b }
    }

    /// The colour's pixel in BGRA order.
    pub const fn bgra(self) -> [u8; 4] {
        [self.b, self.g, self.r, 0xff]
    }

    /// Parses a colour name (`red`, `grey`, …) or hex (`#rgb`, `#rrggbb`, with or without `#`).
    pub fn parse(s: &str) -> Result<Colour> {
        let s = s.trim();
        if let Some(&(_, c)) = NAMED.iter().find(|(name, _)| name.eq_ignore_ascii_case(s)) {
            return Ok(c);
        }
        let hex = s.strip_prefix('#').unwrap_or(s);
        let digits: Option<Vec<u8>> = hex.chars().map(|c| c.to_digit(16).map(|d| d as u8)).collect();
        match digits.as_deref() {
            Some(&[r, g, b]) => Ok(Colour::rgb(r * 17, g * 17, b * 17)),
            Some(&[r1, r0, g1, g0, b1, b0]) => Ok(Colour::rgb(r1 << 4 | r0, g1 << 4 | g0, b1 << 4 | b0)),
            _ => {
                let names: Vec<&str> = NAMED.iter().map(|(n, _)| *n).collect();
                Err(Error::Invalid(format!("unknown colour {s:?}: use #rrggbb, #rgb or one of {}", names.join(", "))))
            }
        }
    }
}

const NAMED: &[(&str, Colour)] = &[
    ("black", Colour::rgb(0, 0, 0)),
    ("white", Colour::rgb(255, 255, 255)),
    ("red", Colour::rgb(255, 0, 0)),
    ("green", Colour::rgb(0, 255, 0)),
    ("blue", Colour::rgb(0, 0, 255)),
    ("yellow", Colour::rgb(255, 255, 0)),
    ("cyan", Colour::rgb(0, 255, 255)),
    ("magenta", Colour::rgb(255, 0, 255)),
    ("orange", Colour::rgb(255, 165, 0)),
    ("grey", Colour::rgb(128, 128, 128)),
    ("gray", Colour::rgb(128, 128, 128)),
];

/// The width and height of one glyph, in font pixels.
pub const GLYPH: u32 = 8;

/// A BGRA image being drawn.
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl Canvas {
    pub fn new(width: u32, height: u32, background: Colour) -> Canvas {
        let bgra = background.bgra().repeat(width as usize * height as usize);
        Canvas { width, height, bgra }
    }

    /// Fills a rectangle, clipped to the canvas.
    pub fn rect(&mut self, x: u32, y: u32, w: u32, h: u32, colour: Colour) {
        let (x1, y1) = (x.saturating_add(w).min(self.width), y.saturating_add(h).min(self.height));
        if x >= x1 || y >= y1 {
            return;
        }
        let px = colour.bgra();
        let stride = self.width as usize * 4;
        for row in self.bgra.chunks_exact_mut(stride).take(y1 as usize).skip(y as usize) {
            for p in row[x as usize * 4..x1 as usize * 4].as_chunks_mut::<4>().0 {
                p.copy_from_slice(&px);
            }
        }
    }

    /// Draws `text` with its top-left corner at (x, y), each font pixel `scale` pixels square.
    /// Characters outside ASCII draw as `?`.
    pub fn text(&mut self, x: u32, y: u32, scale: u32, text: &str, colour: Colour) {
        let scale = scale.max(1);
        for (i, ch) in text.chars().enumerate() {
            let gx = x.saturating_add(i as u32 * GLYPH * scale);
            if gx >= self.width {
                break;
            }
            for (row, bits) in glyph(ch).iter().enumerate() {
                for col in 0..GLYPH {
                    // The font stores each row's leftmost pixel in bit 0.
                    if bits >> col & 1 == 1 {
                        self.rect(gx + col * scale, y + row as u32 * scale, scale, scale, colour);
                    }
                }
            }
        }
    }

    /// Draws lines of text centred on the canvas, at the largest scale (up to `max_scale`) at
    /// which every line fits; lines are wrapped at spaces when even scale 1 is too wide.
    pub fn centred_text(&mut self, text: &str, colour: Colour, max_scale: u32) {
        let (lines, scale) = layout(text, self.width, self.height, max_scale);
        let line_h = GLYPH * scale + scale * 2;
        let total = line_h * lines.len() as u32;
        let mut y = self.height.saturating_sub(total) / 2;
        for line in &lines {
            let w = line.chars().count() as u32 * GLYPH * scale;
            self.text(self.width.saturating_sub(w) / 2, y + scale, scale, line, colour);
            y += line_h;
        }
    }
}

/// The 8 rows of a glyph.
pub fn glyph(ch: char) -> [u8; 8] {
    let i = if ch.is_ascii() && !ch.is_ascii_control() { ch as usize } else { '?' as usize };
    font8x8::legacy::BASIC_LEGACY[i]
}

/// Splits text into lines (at `\n`, then at spaces to fit) and picks the largest scale at which
/// they fit a `width`x`height` box with a margin.
fn layout(text: &str, width: u32, height: u32, max_scale: u32) -> (Vec<String>, u32) {
    let usable_w = width.saturating_sub(width / 10).max(GLYPH);
    let usable_h = height.saturating_sub(height / 10).max(GLYPH);
    for scale in (1..=max_scale.max(1)).rev() {
        let cols = (usable_w / (GLYPH * scale)).max(1) as usize;
        let lines = wrap(text, cols);
        let line_h = GLYPH * scale + scale * 2;
        let fits_w = lines.iter().all(|l| l.chars().count() <= cols);
        if fits_w && line_h * lines.len() as u32 <= usable_h {
            return (lines, scale);
        }
    }
    let cols = (usable_w / GLYPH).max(1) as usize;
    (wrap(text, cols), 1)
}

/// Wraps each `\n`-separated line at spaces to at most `cols` characters, hard-breaking words
/// longer than a line.
fn wrap(text: &str, cols: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let mut word: Vec<char> = word.chars().collect();
            while word.len() > cols {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                out.push(word.drain(..cols).collect());
            }
            let word: String = word.into_iter().collect();
            let needed = if line.is_empty() { word.chars().count() } else { line.chars().count() + 1 + word.chars().count() };
            if needed > cols && !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&word);
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_parse_by_name_and_hex() {
        assert_eq!(Colour::parse("Red"), Ok(Colour::rgb(255, 0, 0)));
        assert_eq!(Colour::parse("#0f8"), Ok(Colour::rgb(0, 255, 136)));
        assert_eq!(Colour::parse("102030"), Ok(Colour::rgb(0x10, 0x20, 0x30)));
        assert_eq!(Colour::parse("grey"), Colour::parse("gray"));
        let err = Colour::parse("mauve").expect_err("must fail").to_string();
        assert!(err.contains("#rrggbb") && err.contains("magenta"), "{err}");
        assert!(Colour::parse("#12345").is_err());
    }

    #[test]
    fn rect_clips_to_the_canvas() {
        let mut c = Canvas::new(4, 4, Colour::BLACK);
        c.rect(2, 2, 10, 10, Colour::WHITE);
        let white = c.bgra.as_chunks::<4>().0.iter().filter(|p| **p == Colour::WHITE.bgra()).count();
        assert_eq!(white, 4);
    }

    #[test]
    fn text_draws_the_font_glyphs() {
        let mut c = Canvas::new(16, 8, Colour::BLACK);
        c.text(0, 0, 1, "A?", Colour::WHITE);
        for (y, bits) in glyph('A').iter().enumerate() {
            for x in 0..8 {
                let i = (y * 16 + x) * 4;
                assert_eq!(c.bgra[i] == 255, bits >> x & 1 == 1, "pixel {x},{y}");
            }
        }
        assert_eq!(glyph('é'), glyph('?'));
        assert_ne!(glyph('A'), glyph('B'));
    }

    #[test]
    fn long_text_wraps_and_shrinks_to_fit() {
        assert_eq!(wrap("hello brave new world", 11), vec!["hello brave", "new world"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
        let (lines, scale) = layout("hi", 640, 480, 8);
        assert_eq!((lines.len(), scale), (1, 8));
        let (lines, scale) = layout("the quick brown fox jumps over the lazy dog", 64, 64, 8);
        assert_eq!(scale, 1);
        assert!(lines.iter().all(|l| l.len() <= 7), "{lines:?}");
    }
}
