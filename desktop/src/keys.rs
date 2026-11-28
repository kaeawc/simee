//! Maps host keyboard events to device HID usages and window shortcuts.

use rune::{Key, KeyEvent};
use sim_core::hid::{KEY_RETURN, KeyStroke, keystroke};

/// What a host key press does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyAction {
    /// Type these strokes on the device keyboard.
    Strokes(Vec<KeyStroke>),
    /// ⌘⇧H: home button.
    Home,
    /// ⌘L: lock button.
    Lock,
    /// ⌘S: save a screenshot.
    Screenshot,
    /// Not handled; let the window use it.
    Ignore,
}

fn plain(usage: u32) -> KeyAction {
    KeyAction::Strokes(vec![KeyStroke { usage, shift: false }])
}

/// Maps one host key press.
pub fn map(event: &KeyEvent) -> KeyAction {
    match &event.key {
        // ⌘⌥ chords are the window's (⌘⌥S, ⌘⌥I: the panes), not the device's.
        Key::Command(_) if event.mods.alt => KeyAction::Ignore,
        Key::Command(c) => match (c, event.mods.shift) {
            ('h', true) => KeyAction::Home,
            ('l', false) => KeyAction::Lock,
            ('s', false) => KeyAction::Screenshot,
            _ => KeyAction::Ignore,
        },
        Key::Text(text) => {
            let strokes: Vec<KeyStroke> = text.chars().filter_map(keystroke).collect();
            if strokes.is_empty() { KeyAction::Ignore } else { KeyAction::Strokes(strokes) }
        }
        Key::Enter => plain(KEY_RETURN),
        Key::Backspace => plain(0x2a),
        Key::Tab => plain(0x2b),
        Key::Escape => plain(0x29),
        Key::Delete => plain(0x4c),
        Key::Right => plain(0x4f),
        Key::Left => plain(0x50),
        Key::Down => plain(0x51),
        Key::Up => plain(0x52),
        Key::Home => plain(0x4a),
        Key::End => plain(0x4d),
        Key::PageUp => plain(0x4b),
        Key::PageDown => plain(0x4e),
        _ => KeyAction::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::Mods;

    fn ev(key: Key, shift: bool) -> KeyEvent {
        let mut e = KeyEvent::from(key);
        e.mods = Mods { shift, ..Mods::default() };
        e
    }

    #[test]
    fn text_becomes_strokes_with_shift() {
        assert_eq!(
            map(&ev(Key::Text("aB".into()), false)),
            KeyAction::Strokes(vec![KeyStroke { usage: 0x04, shift: false }, KeyStroke { usage: 0x05, shift: true }])
        );
    }

    #[test]
    fn named_keys_and_shortcuts() {
        assert_eq!(map(&ev(Key::Enter, false)), plain(KEY_RETURN));
        assert_eq!(map(&ev(Key::Left, false)), plain(0x50));
        assert_eq!(map(&ev(Key::Command('h'), true)), KeyAction::Home);
        assert_eq!(map(&ev(Key::Command('h'), false)), KeyAction::Ignore);
        assert_eq!(map(&ev(Key::Command('l'), false)), KeyAction::Lock);
        assert_eq!(map(&ev(Key::Command('s'), false)), KeyAction::Screenshot);
        let mut cmd_alt_s = ev(Key::Command('s'), false);
        cmd_alt_s.mods.alt = true;
        assert_eq!(map(&cmd_alt_s), KeyAction::Ignore, "⌘⌥S is the window's");
        assert_eq!(map(&ev(Key::Text("é".into()), false)), KeyAction::Ignore);
    }
}

#[cfg(test)]
mod label_tests {
    #[test]
    fn runtime_labels() {
        assert_eq!(crate::view::runtime_label("com.apple.CoreSimulator.SimRuntime.iOS-26-5"), "iOS 26.5");
        assert_eq!(crate::view::runtime_label("com.apple.CoreSimulator.SimRuntime.iOS-27-2-iPhone19-4"), "iOS 27.2.iPhone19.4");
        assert_eq!(crate::view::runtime_label("weird"), "weird");
        assert_eq!(crate::view::runtime_label("system-images;android-36;google_apis;arm64-v8a"), "Android 36 · google_apis");
    }
}
