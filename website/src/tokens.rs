//! The site's tokens: a typeset paper. White page, black ink, CMU Serif (Computer Modern) for
//! text and IBM Plex Mono for code, rules instead of boxes, and the gem's green only for links and
//! the mark. Keep `index.html`'s CSS (the fonts, `--rune-*`) in step with these.

use std::cell::Cell;

use rune::{Color, TextStyle, oklch};

thread_local! {
    /// Whether this view is dark: set once per build of the view (`view`, from the reader's
    /// choice or the system's), read by every colour below.
    static DARK: Cell<bool> = const { Cell::new(false) };
}

pub fn set_dark(dark: bool) {
    DARK.set(dark);
}

pub fn dark() -> bool {
    DARK.get()
}

fn pick(light: Color, dark_: Color) -> Color {
    if dark() { dark_ } else { light }
}

/// The page. Dark is black: the static page's dark mode inverts the light export (index.html),
/// and black is what white inverts to, so Rune's first frame matches it.
pub fn bg() -> Color {
    pick(Color::WHITE, Color::BLACK)
}
/// Body text: about 15:1 on its page in both themes.
pub fn fg() -> Color {
    pick(oklch(0.2, 0.0, 0.0), oklch(0.92, 0.0, 0.0))
}
/// Captions and secondary text: about 7:1.
pub fn fg2() -> Color {
    pick(oklch(0.45, 0.0, 0.0), oklch(0.74, 0.0, 0.0))
}
/// The rules under the header and over the footer.
pub fn rule() -> Color {
    fg()
}
/// Leader lines in Figure 1, table rules, frames.
pub fn rule_faint() -> Color {
    pick(oklch(0.78, 0.0, 0.0), oklch(0.36, 0.0, 0.0))
}
/// Links: the gem's green, at about 5:1 on its page.
pub fn link() -> Color {
    pick(oklch(0.46, 0.1, 165.0), oklch(0.8, 0.12, 165.0))
}
pub fn code_bg() -> Color {
    pick(oklch(0.97, 0.004, 165.0), oklch(0.19, 0.01, 165.0))
}

/// The text column: about 70 characters of 17 pt CMU Serif. Paragraphs stop here.
pub const MEASURE: f32 = 680.0;
/// The page's frame, half again the text column, for tables and figures where the browser has
/// the room.
pub const PAGE: f32 = MEASURE * 1.5;

pub const SERIF: &str = "CMU Serif";
pub const MONO: &str = "IBM Plex Mono";

pub const BODY: TextStyle = TextStyle::new(17.0, 400.0).lh(26.0);
pub const SMALL: TextStyle = TextStyle::new(15.0, 400.0).lh(22.0);
pub const BOLD: TextStyle = TextStyle::new(17.0, 700.0).lh(26.0);
pub const MONO_12: TextStyle = TextStyle::new(12.0, 400.0).lh(16.0).mono();
pub const MONO_13: TextStyle = TextStyle::new(13.0, 400.0).lh(20.0).mono();

/// The page's breakpoints, read from its width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bp {
    /// The widest breakpoint at or under the page's width.
    pub at: u16,
}

impl Bp {
    pub const STEPS: [u16; 3] = [0, 480, 760];
    /// The widths the static page is exported at (`scripts/build.sh`): every breakpoint, with
    /// the narrowest phone for the base, and the page's whole frame (`PAGE` in its gutters),
    /// where Markdown tables, sized in the column they have, reach the frame's width.
    pub const EXPORT: &str = "360,480,760,1084";

    pub fn of(width: f32) -> Bp {
        let at = Self::STEPS.iter().rev().copied().find(|&s| width >= f32::from(s)).unwrap_or(0);
        Bp { at }
    }
    /// Under 480: a phone.
    pub fn phone(self) -> bool {
        self.at < 480
    }
    pub fn gutter(self) -> f32 {
        if self.phone() { 16.0 } else { 32.0 }
    }
    /// The home page's one-word name: "simee".
    pub fn display(self) -> TextStyle {
        if self.phone() { TextStyle::new(44.0, 700.0).lh(52.0) } else { TextStyle::new(64.0, 700.0).lh(72.0) }
    }
    /// The paper's title.
    pub fn title(self) -> TextStyle {
        if self.phone() { TextStyle::new(24.0, 700.0).lh(30.0) } else { TextStyle::new(30.0, 700.0).lh(38.0) }
    }
    /// A numbered section's heading: "1 Performance".
    pub fn h2(self) -> TextStyle {
        if self.phone() { TextStyle::new(19.0, 700.0).lh(26.0) } else { TextStyle::new(21.0, 700.0).lh(28.0) }
    }
}
