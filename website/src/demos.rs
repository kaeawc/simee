//! Demos: emu and the SDK's emulator live, side by side, then the desktop window as figures.
//!
//! The live figures are boxes this page keys `live-emu` and `live-sdk`; `public/live.js` lays a
//! video over each and plays the relay's stream for it (`website/media`, fed by
//! `scripts/live-demo.sh`). Rune's canvas can't hold a video, so the box is all Rune draws.

use rune::{Cx, El, col, el, row, text};

use crate::tokens::*;
use crate::ui::{centered, link_text, prose, section};
use crate::{State, shot};

/// Whether the page shows the live demo. Off until the relay is set up and a demo Mac streams
/// (website/media/README.md); `public/live.js` does nothing without the figures.
const LIVE: bool = false;

/// A phone's screen, width over height: the live boxes' shape before a stream says otherwise
/// (the video keeps its own aspect inside).
const PHONE: f32 = 1080.0 / 2400.0;

pub fn page(s: &State, cx: &Cx<State>, bp: Bp) -> El {
    let figure = |n: &str, name: &str, alt: &str, cap: &str| {
        col()
            .key(format!("fig{n}"))
            .role("figure")
            .w_full()
            .mt(28.0)
            .child(shot(s, cx, name, alt, 2480.0 / 1740.0))
            .child(centered(&format!("fig{n}-cap"), &format!("Figure {n}. {cap}"), SMALL, fg2()).mt(8.0))
    };
    col()
        .key("demos")
        .w_full()
        .pt(if bp.phone() { 28.0 } else { 44.0 })
        .child(text("Demos").key("demos-h1").role("heading").level(1).font(bp.title()).color(fg()).wrap())
        .child(if LIVE { live(bp) } else { col().key("live-off") })
        .child(
            section("window", if LIVE { "2" } else { "" }, "The desktop window", bp)
                .child(figure(
                    if LIVE { "2" } else { "1" },
                    "desktop.png",
                    "The desktop window with iOS and Android devices",
                    "iOS and Android devices in one window, with the iOS debug panel.",
                ))
                .child(figure(
                    if LIVE { "3" } else { "2" },
                    "desktop-android.png",
                    "The desktop window with an Android device",
                    "An Android device on emu, simee's emulator.",
                ))
                .child(link_text("demos-more", "The desktop window's controls and shortcuts", "/docs/desktop/", BODY).mt(24.0)),
        )
}

fn live(bp: Bp) -> El {
    let screen = |key: &str, label: &str, who: &str| {
        let w = if bp.phone() { 150.0 } else { 240.0 };
        col()
            .key(format!("{key}-fig"))
            .items_center()
            .gap(8.0)
            .child(el().key(key.to_owned()).role("figure").label(format!("{who}, streaming live")).size(w, w / PHONE).fill(fg()))
            .child(text(label.to_owned()).key(format!("{key}-label")).font(SMALL).color(fg()))
    };
    section("live", "1", "Live", bp)
        .child(prose(
            "live-p",
            "The same AVD under emu and under the Android SDK's emulator, on one Mac, taking the same input. Each screen is the device's own `screenrecord` stream.",
            BODY,
            fg(),
        ))
        .child(
            row()
                .key("live-row")
                .w_full()
                .justify_center()
                .gap(if bp.phone() { 12.0 } else { 32.0 })
                .mt(20.0)
                .items_start()
                .child(screen("live-emu", "emu (simee)", "emu"))
                .child(screen("live-sdk", "SDK emulator", "The SDK emulator")),
        )
        .child(centered("live-cap", "Figure 1. Live, when the demo Mac is streaming; otherwise each screen says offline.", SMALL, fg2()).mt(10.0))
}
