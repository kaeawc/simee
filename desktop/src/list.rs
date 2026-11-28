//! The device list's order, search and one-click boot (#565), for a host with dozens of devices.
//!
//! - Booted devices, and those booting (the service says so, or a boot from here is running),
//!   come first under their own "Booted" header, above the platform and runtime groups.
//! - The search field at the top of the sidebar (⌘K anywhere, ⌘F while the list has focus)
//!   matches fuzzily ([`sim_core::fuzzy`]) on the name, device type, runtime, platform,
//!   sub-platform, form factor and the UDID or AVD name. While a query is typed the groups
//!   collapse to the ranked matches: booted ones first, then the rest, best first. ↑ and ↓ move
//!   through them, Return acts on the one highlighted as a click would, Escape clears the query
//!   (and, when it is already empty, leaves the field).
//! - A click on a shut-down device, or Return on its row, selects and boots it, past admission
//!   as the Boot button is ([`crate::admission`]); ⌥ selects only.

use rune::{Key, KeyEvent, Tx};
use sim_core::device::{DeviceInfo, DeviceState};
use sim_core::fuzzy::{self, Field};
use sim_core::platform::Platform;

use crate::{Action, State};

/// The search field's key (⌘K focuses it).
pub const SEARCH_KEY: &str = "device-search";

/// One line of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// A section's header: "Booted", a platform, or the matches that aren't running.
    Section { key: String, label: String },
    /// A runtime's header within its platform ("iOS 26.5").
    Runtime(String),
    /// `State::devices[i]`.
    Device(usize),
    /// The query matches no device.
    NoMatches,
}

/// Whether `d` goes in the "Booted" section: running, starting, or booting from here.
pub fn up(s: &State, d: &DeviceInfo) -> bool {
    matches!(d.state, DeviceState::Booted | DeviceState::Booting) || s.jobs.booting(&d.udid)
}

/// How well `d` matches the query `terms`, `None` when it doesn't.
pub fn score(d: &DeviceInfo, terms: &[String]) -> Option<u32> {
    let runtime = crate::view::runtime_label(&d.runtime_id);
    let device_type = d.device_type_id.rsplit('.').next().unwrap_or(&d.device_type_id);
    let form_factor = d.form_factor.map(|f| f.as_str());
    let sub_platform = d.sub_platform.map(|p| p.label());
    let mut fields = vec![Field::words(&d.name), Field::words(device_type), Field::words(&runtime), Field::words(d.platform.label())];
    fields.extend(form_factor.map(Field::words));
    fields.extend(sub_platform.map(Field::words));
    // An AVD's id is its name, matched as one; an iOS UDID only as a run of its characters.
    fields.push(if d.platform == Platform::Android { Field::words(&d.udid) } else { Field::id(&d.udid) });
    fuzzy::score(terms, &fields)
}

/// The list's lines, in order.
pub fn rows(s: &State) -> Vec<Row> {
    let terms = fuzzy::terms(&s.search.text);
    let mut rows = Vec::new();
    let booted = Row::Section { key: "group-booted".into(), label: "Booted".into() };
    if !terms.is_empty() {
        let mut found: Vec<(usize, bool, u32)> =
            s.devices.iter().enumerate().filter_map(|(i, d)| score(d, &terms).map(|sc| (i, up(s, d), sc))).collect();
        found.sort_by_key(|&(i, up, sc)| (!up, std::cmp::Reverse(sc), i));
        if found.is_empty() {
            return vec![Row::NoMatches];
        }
        let split = found.iter().position(|&(_, up, _)| !up).unwrap_or(found.len());
        if split > 0 {
            rows.push(booted);
            rows.extend(found[..split].iter().map(|&(i, ..)| Row::Device(i)));
        }
        if split < found.len() {
            rows.push(Row::Section { key: "group-matches".into(), label: "Not running".into() });
            rows.extend(found[split..].iter().map(|&(i, ..)| Row::Device(i)));
        }
        return rows;
    }
    let (mut running, mut rest): (Vec<usize>, Vec<usize>) = (0..s.devices.len()).partition(|&i| up(s, &s.devices[i]));
    let ios_first = |i: &usize| s.devices[*i].platform != Platform::Ios;
    running.sort_by_key(ios_first);
    if !running.is_empty() {
        rows.push(booted);
        rows.extend(running.into_iter().map(Row::Device));
    }
    rest.sort_by_key(|&i| (s.devices[i].platform != Platform::Ios, s.devices[i].runtime_id.clone()));
    let (mut platform, mut runtime) = (None, "");
    for i in rest {
        let d = &s.devices[i];
        if platform != Some(d.platform) {
            platform = Some(d.platform);
            runtime = "";
            rows.push(Row::Section { key: format!("platform-{}", d.platform.as_str()), label: d.platform.label().into() });
        }
        if d.runtime_id != runtime {
            runtime = &d.runtime_id;
            rows.push(Row::Runtime(crate::view::runtime_label(runtime)));
        }
        rows.push(Row::Device(i));
    }
    rows
}

/// The devices as the list shows them, top to bottom (`State::devices` indices): what ↑ and ↓
/// move through.
pub fn order(s: &State) -> Vec<usize> {
    rows(s).into_iter().filter_map(|r| if let Row::Device(i) = r { Some(i) } else { None }).collect()
}

/// The device ↑ and ↓ have reached (a `State::devices` index), while the search field has
/// focus or a query: the one `State::search_highlight` names wherever the list has since moved
/// it (a boot finishing, a rescan), else the first match.
pub fn highlighted(s: &State) -> Option<usize> {
    if !s.search.focused && s.search.text.trim().is_empty() {
        return None;
    }
    let order = order(s);
    let kept = s.search_highlight.as_deref().and_then(|u| order.iter().copied().find(|&i| s.devices[i].udid == u));
    kept.or_else(|| order.first().copied())
}

/// Pins the highlight to a device while the search is in use: the first match as it's first
/// shown (a query typed, the field focused), so a rescan that lists a better match before
/// Return (a farm lane's new AVD) doesn't change what Return boots (#571). A pinned device
/// that left the list gives way to the first one shown. Out of use, nothing is pinned.
pub fn keep_highlight(s: &mut State) {
    if !s.search.focused && s.search.text.trim().is_empty() {
        s.search_highlight = None;
        return;
    }
    let order = order(s);
    let listed = |u: &str| order.iter().any(|&i| s.devices[i].udid == u);
    if !s.search_highlight.as_deref().is_some_and(listed) {
        s.search_highlight = order.first().map(|&i| s.devices[i].udid.clone());
    }
}

/// The query changed: the highlight starts again on its first match.
pub fn query_changed(s: &mut State) {
    s.search_highlight = None;
    keep_highlight(s);
}

/// Moves the highlight `by` places through the list's order, stopping at either end.
fn step(s: &mut State, by: isize) {
    let order = order(s);
    let Some(last) = order.len().checked_sub(1) else { return };
    let at = highlighted(s).and_then(|i| order.iter().position(|&o| o == i)).unwrap_or(0);
    let to = at.saturating_add_signed(by).min(last);
    s.search_highlight = Some(s.devices[order[to]].udid.clone());
}

/// A click on a row, or Return on it: selects the device, and boots it when it is shut down
/// (past admission, by the Boot button's path); `select_only` (⌥) never boots. A device out in
/// its own window is only brought forward there, even when it has stopped: its window says so,
/// and closing it brings the device back here to boot.
pub fn activate(s: &mut State, udid: &str, select_only: bool) {
    let boots = !select_only
        && !s.popouts.is_open(udid)
        && s.jobs.label(udid).is_none()
        && s.devices.iter().any(|d| d.udid == udid && d.state == DeviceState::Shutdown);
    if !select_only && s.popouts.is_open(udid) && s.devices.iter().any(|d| d.udid == udid && d.state == DeviceState::Shutdown) {
        s.status = Some(format!("{} stopped in its own window: close it to boot it here", s.device_name(udid)));
    }
    s.actions.push(Action::Select(udid.to_owned()));
    if boots {
        s.actions.push(Action::Boot(udid.to_owned()));
    }
}

/// The search field's keys, before its editor: ↑ ↓ move through the matches, Return acts on
/// the highlighted one (⌥Return selects only), Escape clears the query or leaves the field.
pub fn search_key(s: &mut State, k: &KeyEvent, tx: &mut Tx) -> bool {
    match k.key {
        Key::Escape if s.search.text.is_empty() => tx.blur(),
        Key::Escape => {
            s.search.clear();
            s.search_highlight = None;
        }
        Key::Down => step(s, 1),
        Key::Up => step(s, -1),
        Key::Enter => {
            let Some(i) = highlighted(s) else { return true };
            let udid = s.devices[i].udid.clone();
            activate(s, &udid, k.mods.alt);
            tx.request_poll();
        }
        _ => return false,
    }
    true
}

/// The window's keys for the list: ⌘K focuses the search field from anywhere, ⌘F while the
/// list has focus (elsewhere ⌘F is the 3D view's flip).
pub fn on_key(_: &mut State, k: &KeyEvent, tx: &mut Tx) -> bool {
    let plain = !k.mods.shift && !k.mods.alt;
    let find = k.key == Key::Command('f') && tx.focus_within("device-list");
    if plain && (k.key == Key::Command('k') || find) {
        tx.focus(SEARCH_KEY, true);
        return true;
    }
    false
}

/// The headless `list-booted` scene: the demo list with a phone of each platform running and an
/// AVD booting from here, all at the top under "Booted".
pub fn booted_scene(s: &mut State) {
    crate::demo(s);
    s.devices[3].state = DeviceState::Booting;
    // A boot from here that doesn't finish while the scene draws: its row and the stage show it.
    let (never, wait) = std::sync::mpsc::channel::<()>();
    let _ = s.jobs.start(
        "pixel_fold",
        crate::jobs::BOOTING,
        Box::new(move || {
            wait.recv().ok();
            Ok(())
        }),
        None,
    );
    std::mem::forget(never);
    s.selected = Some("pixel_fold".into());
    s.offline_screen = false;
}

/// The headless `list-search` scene: "17 26.5" typed, the iPhone 17s on iOS 26.5 matched, the
/// second highlighted.
pub fn search_scene(s: &mut State) {
    crate::demo(s);
    s.search = rune::TextEdit::new("17 26.5");
    s.search_highlight = Some("B2".into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_core::form_factor::FormFactor;

    fn state(devices: Vec<DeviceInfo>) -> State {
        let mut s = crate::init();
        s.offline = true;
        s.actions.clear();
        s.devices = devices;
        s
    }

    fn names(s: &State) -> Vec<String> {
        order(s).into_iter().map(|i| s.devices[i].name.clone()).collect()
    }

    fn demo() -> State {
        let mut s = crate::init();
        crate::demo(&mut s);
        s
    }

    /// Booted and booting devices come first, under their own header, above every platform and
    /// runtime group; the rest keep their groups.
    #[test]
    fn booted_and_booting_devices_are_listed_first() {
        let mut s = demo();
        s.devices[3].state = DeviceState::Booting; // iPhone 18 Pro, iOS 27.0
        let rows = rows(&s);
        assert_eq!(rows[0], Row::Section { key: "group-booted".into(), label: "Booted".into() });
        let first: Vec<&str> = rows[1..4].iter().map(|r| if let Row::Device(i) = r { s.devices[*i].name.as_str() } else { "-" }).collect();
        assert_eq!(first, ["iPhone 17", "iPhone 18 Pro", "pixel_9"], "booted, booting, then Android's booted");
        assert_eq!(rows[4], Row::Section { key: "platform-ios".into(), label: "iOS".into() });
        assert_eq!(rows[5], Row::Runtime("iOS 26.5".into()));
        let android = rows.iter().position(|r| matches!(r, Row::Section { key, .. } if key == "platform-android")).expect("android group");
        assert!(!rows[android..].contains(&Row::Device(0)), "a booted device isn't listed again in its group");
        // A boot from here counts before the service says so.
        let (_gate, wait) = std::sync::mpsc::channel::<()>();
        s.jobs
            .start(
                "wear_round",
                crate::jobs::BOOTING,
                Box::new(move || {
                    wait.recv().ok();
                    Ok(())
                }),
                None,
            )
            .expect("start");
        assert_eq!(names(&s)[..4], ["iPhone 17", "iPhone 18 Pro", "pixel_9", "wear_round"]);
    }

    fn avd(name: &str, form_factor: FormFactor, state: DeviceState) -> DeviceInfo {
        crate::android_info(name, form_factor, "android-36;google_apis", state)
    }

    #[test]
    fn pix9_finds_acceptance_pixel_9_first() {
        let mut s = state(vec![
            avd("pixel_fold", FormFactor::Foldable, DeviceState::Shutdown),
            avd("amslot-19-pixel", FormFactor::Phone, DeviceState::Shutdown),
            avd("acceptance-pixel-9", FormFactor::Phone, DeviceState::Shutdown),
            avd("pixel_tablet", FormFactor::Tablet, DeviceState::Shutdown),
        ]);
        s.search = rune::TextEdit::new("pix9");
        assert_eq!(names(&s), ["acceptance-pixel-9"]);
        s.search = rune::TextEdit::new("pix");
        assert_eq!(names(&s), ["pixel_fold", "pixel_tablet", "amslot-19-pixel", "acceptance-pixel-9"], "prefixes rank above word starts");
    }

    #[test]
    fn seventeen_on_26_5_finds_the_iphone_17s_on_ios_26_5() {
        let mut s = demo();
        s.search = rune::TextEdit::new("17 26.5");
        assert_eq!(names(&s), ["iPhone 17", "iPhone 17 Pro Max"]);
        assert_eq!(rows(&s)[0], Row::Section { key: "group-booted".into(), label: "Booted".into() }, "the booted one first");
        s.search = rune::TextEdit::new("watch");
        assert_eq!(names(&s), ["Apple Watch Series 10 (46mm)", "wear_round"], "a form factor matches too");
        s.search = rune::TextEdit::new("nothing-like-it");
        assert_eq!(rows(&s), [Row::NoMatches]);
    }

    /// Booted devices stay above better matches that aren't running.
    #[test]
    fn booted_matches_rank_above_the_rest() {
        let mut s = state(vec![
            avd("pixel", FormFactor::Phone, DeviceState::Shutdown),
            avd("old-pixel-phone", FormFactor::Phone, DeviceState::Booted),
        ]);
        s.search = rune::TextEdit::new("pixel");
        assert_eq!(names(&s), ["old-pixel-phone", "pixel"]);
    }

    #[test]
    fn a_shut_down_device_boots_on_click_and_option_selects_only() {
        let mut s = demo();
        activate(&mut s, "B2", false);
        assert_eq!(s.actions, [Action::Select("B2".into()), Action::Boot("B2".into())]);
        s.actions.clear();
        activate(&mut s, "B2", true);
        assert_eq!(s.actions, [Action::Select("B2".into())], "⌥ selects only");
        s.actions.clear();
        activate(&mut s, "A1", false);
        assert_eq!(s.actions, [Action::Select("A1".into())], "a booted device is only selected");
    }
}
