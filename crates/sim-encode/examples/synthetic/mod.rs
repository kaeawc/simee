//! Synthetic screens for the tests and the Linux benchmark: no device needed, deterministic, and
//! shaped like what a phone shows (flat colour, sharp edges, text-like detail, a scroll), not like
//! camera video.

use simee_encode::{Picture, PixelOrder};

/// A tiny integer hash, so the same cell always gets the same colour.
fn hash(a: u32, b: u32) -> u32 {
    let mut h = a.wrapping_mul(0x9e37_79b1) ^ b.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^ (h >> 13)
}

/// What the picture shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Content {
    /// An app drawer: a gradient wallpaper, a grid of coloured icons with text-like labels,
    /// scrolled by `scroll` pixels.
    Drawer,
    /// Every pixel different on every frame: the worst case for any encoder.
    Noise,
}

/// Draws one frame into `data` (resized to fit; reused when it already is the right size).
pub fn render(content: Content, width: u32, height: u32, scroll: u32, order: PixelOrder, data: &mut Vec<u8>) {
    data.resize(width as usize * height as usize * 4, 0);
    let (ri, bi) = match order {
        PixelOrder::Bgra => (2, 0),
        PixelOrder::Rgba => (0, 2),
    };
    let cell = (width / 7).max(32);
    for (y, row) in data.chunks_exact_mut(width as usize * 4).enumerate() {
        let wy = y as u32 + scroll;
        let (r, dy) = (wy / cell, wy % cell);
        // The wallpaper doesn't scroll.
        let shade = (y as u32 * 90 / height.max(1)) as u8;
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = x as u32;
            let rgb = match content {
                Content::Noise => {
                    let h = hash(x + y as u32 * width, scroll);
                    [h as u8, (h >> 8) as u8, (h >> 16) as u8]
                }
                Content::Drawer => {
                    let (c, dx) = (x / cell, x % cell);
                    let h = hash(r, c);
                    let inset = cell / 6;
                    if (inset..cell - inset).contains(&dx) && (inset..cell - inset * 2).contains(&dy) {
                        [(h as u8) | 0x40, ((h >> 8) as u8) | 0x40, ((h >> 16) as u8) | 0x40]
                    } else if (cell - inset * 2 + 8..cell - inset * 2 + 24).contains(&dy)
                        && (inset..cell - inset).contains(&dx)
                        && !(dx / 5 + h).is_multiple_of(3)
                    {
                        [235, 235, 240]
                    } else {
                        [20 + shade / 2, 40 + shade / 3, 90 + shade]
                    }
                }
            };
            px[ri] = rgb[0];
            px[1] = rgb[1];
            px[bi] = rgb[2];
            px[3] = 255;
        }
    }
}

/// A frame as a [`Picture`].
pub fn picture(content: Content, width: u32, height: u32, scroll: u32, order: PixelOrder) -> Picture {
    let mut data = Vec::new();
    render(content, width, height, scroll, order, &mut data);
    Picture::new(width, height, order, data)
}
