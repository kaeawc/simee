//! The app's SVG icons: 24-point stroke glyphs tinted with `currentColor`.

use rune::Icon;

macro_rules! icons {
    ($($name:ident = $id:literal, $body:literal;)*) => {
        $(pub static $name: Icon = Icon { name: concat!("simee-", $id), view: 24.0, body: $body };)*
    };
}

// Every body draws with stroke="currentColor" so `.color()` tints it.
icons! {
    HOME = "home", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 10.5 12 3l9 7.5"/><path d="M5 9.5V20h5v-6h4v6h5V9.5"/></g>"#;
    LOCK = "lock", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="11" width="14" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/></g>"#;
    MOON = "moon", r#"<path d="M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5Z" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/>"#;
    CAMERA = "camera", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M4 8h3l2-3h6l2 3h3a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V9a1 1 0 0 1 1-1Z"/><circle cx="12" cy="13.5" r="3.5"/></g>"#;
    POWER = "power", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M12 3v9"/><path d="M6.3 6.8a8 8 0 1 0 11.4 0"/></g>"#;
    PLAY = "play", r#"<path d="M7 4.5v15l12.5-7.5L7 4.5Z" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/>"#;
    REFRESH = "refresh", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M20 11a8 8 0 0 0-14.6-4.5L3 9"/><path d="M3 4v5h5"/><path d="M4 13a8 8 0 0 0 14.6 4.5L21 15"/><path d="M21 20v-5h-5"/></g>"#;
    SLIDERS = "sliders", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M4 6h10M18 6h2M4 12h4M12 12h8M4 18h12M20 18h0"/><circle cx="16" cy="6" r="2"/><circle cx="10" cy="12" r="2"/><circle cx="18" cy="18" r="2"/></g>"#;
    GLOBE = "globe", r#"<g fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="9"/><path d="M3 12h18"/><path d="M12 3c2.5 2.6 3.8 5.6 3.8 9s-1.3 6.4-3.8 9c-2.5-2.6-3.8-5.6-3.8-9S9.5 5.6 12 3Z"/></g>"#;
    FOLDER = "folder", r#"<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"/>"#;
    CPU = "cpu", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><rect x="6" y="6" width="12" height="12" rx="2"/><rect x="9.5" y="9.5" width="5" height="5" rx="0.5"/><path d="M9 3v3M15 3v3M9 18v3M15 18v3M3 9h3M3 15h3M18 9h3M18 15h3"/></g>"#;
    TERMINAL = "terminal", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="m7 9 3 3-3 3M12.5 15H17"/></g>"#;
    TRASH = "trash", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></g>"#;
    CLOSE = "close", r#"<path d="M6 6l12 12M18 6 6 18" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>"#;
    REVEAL = "reveal", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14 4h6v6M20 4l-9 9"/><path d="M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"/></g>"#;
    FINGERPRINT = "fingerprint", r#"<g fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M6.5 6.5a7.5 7.5 0 0 1 12.5 5.5v1.5"/><path d="M4.5 11a7.5 7.5 0 0 1 1-3.5M4.5 15v-2"/><path d="M8 19.5c.8-2 1-4 1-6.5a3 3 0 0 1 6 0c0 2.8-.3 5.3-1.2 7.5"/><path d="M12 13c0 3-.5 5.5-1.7 7.5M18.5 17.5c-.2 1-.5 2-.9 3"/></g>"#;
    FACE = "face", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 8V5a1 1 0 0 1 1-1h3M16 4h3a1 1 0 0 1 1 1v3M20 16v3a1 1 0 0 1-1 1h-3M8 20H5a1 1 0 0 1-1-1v-3"/><path d="M9 9.5v1M15 9.5v1M12 9.5V13h-1M9.5 16a4 4 0 0 0 5 0"/></g>"#;
    BELL = "bell", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M6 16V11a6 6 0 0 1 12 0v5l1.5 2h-15L6 16Z"/><path d="M10 20.5a2 2 0 0 0 4 0"/></g>"#;
    KEY = "key", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="15" r="4"/><path d="M11 12 20 3M16.5 6.5 19 9M14 9l2 2"/></g>"#;
    ALERT = "alert", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3.5 2.5 20h19L12 3.5Z"/><path d="M12 10v4.5M12 17.5v.01"/></g>"#;
    PIN = "pin", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M12 21s-6.5-6-6.5-11a6.5 6.5 0 0 1 13 0c0 5-6.5 11-6.5 11Z"/><circle cx="12" cy="10" r="2.3"/></g>"#;
    SIGNAL = "signal", r#"<g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M5 19v-3M10 19v-6M15 19V9M20 19V5"/></g>"#;
    TEXT_SIZE = "text-size", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 19 8 6l5 13M4.8 14.5h6.4M15 19l3-7.5 3 7.5M16 16.5h4"/></g>"#;
    MINUS = "minus", r#"<path d="M5 12h14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>"#;
    PLUS = "plus", r#"<path d="M12 5v14M5 12h14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>"#;
    CONTRAST = "contrast", r#"<g fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="9"/><path d="M12 3a9 9 0 0 1 0 18Z" fill="currentColor"/></g>"#;
    KEYBOARD = "keyboard", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><rect x="2.5" y="6" width="19" height="12" rx="2"/><path d="M6 10h.01M10 10h.01M14 10h.01M18 10h.01M6 14h.01M18 14h.01M9 14h6"/></g>"#;
    FACE_ON = "face-on", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 8V4h4M17 4h4v4M21 16v4h-4M7 20H3v-4"/><rect x="8.5" y="6.5" width="7" height="11" rx="1.5"/></g>"#;
    FLIP = "flip", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="8.5" y="4" width="7" height="16" rx="1.5"/><path d="M5 9a8 4 0 0 0 0 6M3.5 13.5 5 15l1.5-1.5M19 15a8 4 0 0 0 0-6M20.5 10.5 19 9l-1.5 1.5"/></g>"#;
    LAYERS = "layers", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="m12 3 9 5-9 5-9-5 9-5Z"/><path d="m3 12 9 5 9-5M3 16l9 5 9-5"/></g>"#;
    WIFI_OFF = "wifi-off", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M3 3l18 18"/><path d="M8.5 16.5a5 5 0 0 1 7 0M5 12.5a10 10 0 0 1 5-2.6M14.3 10a10 10 0 0 1 4.7 2.5M2 8.8a15 15 0 0 1 4.2-2.6M10.5 5.1A15 15 0 0 1 22 8.8"/><path d="M12 20h.01"/></g>"#;
    GAUGE = "gauge", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4.5 18a9 9 0 1 1 15 0"/><path d="m12 13 4-5"/><circle cx="12" cy="13" r="1.2" fill="currentColor"/></g>"#;
    SPEAKER_OFF = "speaker-off", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 9.5h3.5L12 5.5v13l-4.5-4H4z"/><path d="m16 9.5 5 5M21 9.5l-5 5"/></g>"#;
    RECORD = "record", r#"<g fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="8.5"/><circle cx="12" cy="12" r="3.5" fill="currentColor"/></g>"#;
    BACK = "back", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M19 12H5"/><path d="m11 6-6 6 6 6"/></g>"#;
    RECENTS = "recents", r#"<rect x="5" y="5" width="14" height="14" rx="2.5" fill="none" stroke="currentColor" stroke-width="1.8"/>"#;
    ROTATE = "rotate", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M20 12a8 8 0 1 1-2.3-5.7L20 8.5"/><path d="M20 3.5v5h-5"/></g>"#;
    FOLD_CLOSED = "fold-closed", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><rect x="7" y="3" width="10" height="18" rx="2"/><path d="M10 3v18"/></g>"#;
    FOLD_HALF = "fold-half", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 19h9l9-9"/><path d="M12 19V5a1 1 0 0 0-1-1H4a1 1 0 0 0-1 1v14"/></g>"#;
    FOLD_OPEN = "fold-open", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M12 4v16" stroke-dasharray="2 2"/></g>"#;
    CROWN_UP = "crown-up", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M6 14a6 6 0 0 1 12 0"/><path d="m15 6 3 2-3 2"/><path d="M12 14v6"/></g>"#;
    CROWN_DOWN = "crown-down", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M6 10a6 6 0 0 0 12 0"/><path d="m15 18 3-2-3-2"/><path d="M12 10V4"/></g>"#;
    CHEVRON_UP = "chevron-up", r#"<path d="m6 15 6-6 6 6" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"#;
    CHEVRON_DOWN = "chevron-down", r#"<path d="m6 9 6 6 6-6" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"#;
    CHEVRON_LEFT = "chevron-left", r#"<path d="m15 6-6 6 6 6" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"#;
    CHEVRON_RIGHT = "chevron-right", r#"<path d="m9 6 6 6-6 6" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>"#;
    SELECT = "select", r#"<g fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="8.5"/><circle cx="12" cy="12" r="3" fill="currentColor"/></g>"#;
    PEN = "pen", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M15.5 4.5 19.5 8.5 8.5 19.5H4.5v-4L15.5 4.5Z"/><path d="m13 7 4 4"/></g>"#;
    ISLAND = "island", r#"<g fill="none" stroke="currentColor" stroke-width="1.8"><rect x="6" y="2.5" width="12" height="19" rx="3"/><rect x="9.5" y="4.8" width="5" height="1.8" rx="0.9" fill="currentColor"/></g>"#;
    CAR = "car", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M5 11 7 6h10l2 5"/><rect x="3" y="11" width="18" height="6" rx="1.5"/><path d="M6 17v2M18 17v2"/><circle cx="7.5" cy="14" r="0.8" fill="currentColor"/><circle cx="16.5" cy="14" r="0.8" fill="currentColor"/></g>"#;
    // macOS's sidebar.left and sidebar.right: a window with its leading or trailing column.
    SIDEBAR_LEFT = "sidebar-left", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4.5" width="18" height="15" rx="2.5"/><path d="M9.5 4.5v15M5.5 8.5h1.5M5.5 11h1.5"/></g>"#;
    SIDEBAR_RIGHT = "sidebar-right", r#"<g fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4.5" width="18" height="15" rx="2.5"/><path d="M14.5 4.5v15M17 8.5h1.5M17 11h1.5"/></g>"#;
}

/// Every icon, for tests that check each one renders.
#[cfg(test)]
pub static ALL: &[&Icon] = &[
    &HOME,
    &LOCK,
    &MOON,
    &CAMERA,
    &POWER,
    &PLAY,
    &REFRESH,
    &SLIDERS,
    &GLOBE,
    &FOLDER,
    &CPU,
    &TERMINAL,
    &TRASH,
    &CLOSE,
    &REVEAL,
    &FINGERPRINT,
    &FACE,
    &BELL,
    &KEY,
    &ALERT,
    &PIN,
    &SIGNAL,
    &TEXT_SIZE,
    &MINUS,
    &PLUS,
    &CONTRAST,
    &KEYBOARD,
    &LAYERS,
    &FACE_ON,
    &FLIP,
    &ISLAND,
    &BACK,
    &RECENTS,
    &ROTATE,
    &FOLD_CLOSED,
    &FOLD_HALF,
    &FOLD_OPEN,
    &CROWN_UP,
    &CROWN_DOWN,
    &CHEVRON_UP,
    &CHEVRON_DOWN,
    &CHEVRON_LEFT,
    &CHEVRON_RIGHT,
    &SELECT,
    &PEN,
    &CAR,
    &SIDEBAR_LEFT,
    &SIDEBAR_RIGHT,
];
