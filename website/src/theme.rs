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

    /// Whether the page is dark, given the system's scheme.
    pub fn dark(self, system_dark: bool) -> bool {
        match self {
            Theme::System => system_dark,
            Theme::Light => false,
            Theme::Dark => true,
        }
    }
}

/// The reader's stored choice (the web), else the system's.
pub fn load() -> Theme {
    #[cfg(target_arch = "wasm32")]
    {
        let stored = web_sys::window().and_then(|w| w.local_storage().ok().flatten()).and_then(|s| s.get_item(KEY).ok().flatten());
        return stored.map_or(Theme::System, |s| Theme::parse(&s));
    }
    #[cfg(not(target_arch = "wasm32"))]
    Theme::System
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
