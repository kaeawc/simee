//! What the emulator console's commands do to a device, for the front ends that must not let a
//! command through that a tool of its own would ask about: `kill` ends the emulator and
//! `avd snapshot del` deletes a snapshot, whatever else surrounds them in the text. The names
//! here are the ones the stock console accepts, and `emu`'s own console parser matches its
//! commands through this table, so the two can't drift apart (`apps/emu/src/console.rs`).

/// The command that ends the emulator, as stock and `emu` read it.
pub const KILL: &str = "kill";

/// Stock commands that end the emulator more roughly (`emu` doesn't implement them).
pub const STOCK_ENDS: &[&str] = &["crash"];

/// What `avd snapshot` calls its deletion: `emu` answers each of these as a delete.
pub const SNAPSHOT_DELETE: &[&str] = &["del", "delete"];

/// Whether `word`, as typed, is `avd snapshot`'s deletion (the console reads commands in the case
/// they are typed in; [`effects`] lowercases, to ask where the console might not have acted).
pub fn is_snapshot_delete(word: &str) -> bool {
    SNAPSHOT_DELETE.contains(&word)
}

/// What a console command line does that a person may want to be asked about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    /// It ends the emulator (`kill`, `crash`), or stops its guest (`avd stop`).
    pub ends_emulator: bool,
    /// It deletes a snapshot.
    pub deletes_snapshot: bool,
}

impl Effects {
    /// Whether it does either.
    pub fn any(self) -> bool {
        self.ends_emulator || self.deletes_snapshot
    }
}

impl std::ops::BitOr for Effects {
    type Output = Effects;

    fn bitor(self, other: Effects) -> Effects {
        Effects {
            ends_emulator: self.ends_emulator || other.ends_emulator,
            deletes_snapshot: self.deletes_snapshot || other.deletes_snapshot,
        }
    }
}

/// The commands in `text`. The console runs each line as a command of its own, and a line ends
/// at a CR or an LF (stock reads either; `emu` reads LF, and its words split at a CR), so a
/// harmless word before a line break hides nothing after it.
pub fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split(['\r', '\n']).map(str::trim).filter(|line| !line.is_empty())
}

/// What one command line does. Matching ignores case: a command the console reads in any case is
/// asked about, and one it doesn't read is only asked about needlessly.
pub fn effects(line: &str) -> Effects {
    let words: Vec<String> = line.split_whitespace().map(str::to_ascii_lowercase).collect();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    match words.as_slice() {
        [first, ..] if *first == KILL || STOCK_ENDS.contains(first) => Effects { ends_emulator: true, ..Effects::default() },
        ["avd", "stop", ..] => Effects { ends_emulator: true, ..Effects::default() },
        ["avd", "snapshot", name, ..] if is_snapshot_delete(name) => Effects { deletes_snapshot: true, ..Effects::default() },
        _ => Effects::default(),
    }
}

/// What every command in `commands` does, each text taken line by line.
pub fn effects_of<'a>(commands: impl IntoIterator<Item = &'a str>) -> Effects {
    commands.into_iter().flat_map(lines).map(effects).fold(Effects::default(), |all, one| all | one)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENDS: Effects = Effects { ends_emulator: true, deletes_snapshot: false };
    const DELETES: Effects = Effects { ends_emulator: false, deletes_snapshot: true };

    #[test]
    fn a_command_is_what_its_words_say() {
        assert_eq!(effects("kill"), ENDS);
        assert_eq!(effects("  KILL  now"), ENDS);
        assert_eq!(effects("crash"), ENDS);
        assert_eq!(effects("avd stop"), ENDS);
        for name in SNAPSHOT_DELETE {
            assert_eq!(effects(&format!("avd snapshot {name} fresh")), DELETES, "{name}");
        }
        for harmless in
            ["", "ping", "killer", "avd name", "avd snapshot list", "avd snapshot load x", "avd snapshot", "event send EV_KEY:KEY_K:1"]
        {
            assert_eq!(effects(harmless), Effects::default(), "{harmless:?}");
        }
    }

    #[test]
    fn every_line_of_every_command_counts() {
        assert_eq!(effects_of(["ping\r\nkill"]), ENDS);
        assert_eq!(effects_of(["ping\nkill"]), ENDS);
        assert_eq!(effects_of(["ping\rkill"]), ENDS);
        assert_eq!(effects_of(["ping", "help\n\n kill"]), ENDS);
        assert_eq!(effects_of(["kill", "avd snapshot del x"]), ENDS | DELETES);
        assert_eq!(effects_of(["avd snapshot del x\r\nping", "ping"]), DELETES);
        assert!(!effects_of(["ping", "help"]).any());
    }
}
