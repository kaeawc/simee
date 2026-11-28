//! Light and dark: the system's by default, or the reader's choice from the toggle in the
//! running head. The choice is kept in `localStorage` (`simee-theme`) and mirrored onto `<html
//! data-theme>`, which the static page's CSS reads (index.html), so a reload, the static first
//! paint and Rune agree.

/// What the reader chose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Theme {
    /// Follow the system (`prefers-color-scheme`), and change with it.
    #[default]
    System,
    Light,
    Dark,
}

/// The `localStorage` key, shared with index.html's script.
#[cfg(target_arch = "wasm32")]
const KEY: &str = "simee-theme";

impl Theme {
    /// The toggle's order: System, Light, Dark, System.
    pub fn next(self) -> Theme {
        match self {
            Theme::System => Theme::Light,
            Theme::Light => Theme::Dark,
            Theme::Dark => Theme::System,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Theme::System => "System",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
        }
    }

    /// What `localStorage` and `data-theme` hold (the web build reads and writes them).
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn parse(s: &str) -> Theme {
        match s {
            "light" => Theme::Light,
            "dark" => Theme::Dark,
            _ => Theme::System,
        }
    }

    /// The reader's choice from `<html data-theme>`, then from `localStorage`: the first that
    /// names Light or Dark, else System.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn chosen(attr: Option<&str>, stored: Option<&str>) -> Theme {
        [attr, stored].into_iter().flatten().map(Theme::parse).find(|t| *t != Theme::System).unwrap_or(Theme::System)
    }

    /// Whether the page is dark, given the system's scheme.
    pub fn dark(self, system_dark: bool) -> bool {
        match self {
            Theme::System => system_dark,
            Theme::Light => false,
            Theme::Dark => true,
        }
    }
}

/// The reader's choice as the page's head script left it on `<html data-theme>` (it reads
/// `localStorage` before anything paints), else `localStorage`, else the system's. Rune's first
/// frame starts from this, so it never draws the system's theme for a reader who chose one.
pub fn load() -> Theme {
    #[cfg(target_arch = "wasm32")]
    let theme = {
        let window = web_sys::window();
        let attr =
            window.as_ref().and_then(|w| w.document()).and_then(|d| d.document_element()).and_then(|r| r.get_attribute("data-theme"));
        let stored = window.and_then(|w| w.local_storage().ok().flatten()).and_then(|s| s.get_item(KEY).ok().flatten());
        Theme::chosen(attr.as_deref(), stored.as_deref())
    };
    #[cfg(not(target_arch = "wasm32"))]
    let theme = Theme::System;
    theme
}

/// Keeps `theme` for the next page and puts it on `<html data-theme>` (none for System).
pub fn store(theme: Theme) {
    #[cfg(target_arch = "wasm32")]
    {
        let Some(window) = web_sys::window() else { return };
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item(KEY, theme.as_str());
        }
        if let Some(root) = window.document().and_then(|d| d.document_element()) {
            let _ = match theme {
                Theme::System => root.remove_attribute("data-theme"),
                _ => root.set_attribute("data-theme", theme.as_str()),
            };
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = theme;
}
