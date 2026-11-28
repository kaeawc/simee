//! A running device's audio, over adb: the media stream's volume (the guest's VolumeCtrl, `cmd
//! media_session volume` or on older images `media volume`) and mute (`cmd audio adj-mute`, API 35
//! on), and what `dumpsys audio` says about every stream, the ringer and audio focus.
//!
//! The media stream (`STREAM_MUSIC`) is the one apps play through, so it is the one these act on.
//! The guest's audio leaves through emu's sound card, which plays on the host's default output only
//! when the device was booted with `audio` (and otherwise stays silent on the host).

use crate::apps::{Adb, failure};

/// `AudioManager.STREAM_MUSIC`.
pub const MUSIC: u8 = 3;

/// One stream's volume, as `dumpsys audio` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    /// The stream's name without its prefix, lower case: `music`, `ring`, `voice_call`.
    pub name: String,
    pub muted: bool,
    /// The index the stream plays at now (its device's), `None` when the dump doesn't say.
    pub volume: Option<u32>,
    pub min: u32,
    /// `None` when the dump has no `Max:` line.
    pub max: Option<u32>,
    /// The output devices it plays on now (`speaker`).
    pub devices: Vec<String>,
}

/// One entry of the audio focus stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Focus {
    pub package: String,
    pub uid: Option<u32>,
    /// What it asked for: `GAIN`, `GAIN_TRANSIENT`, …
    pub gain: String,
    pub client: String,
}

/// What `dumpsys audio` says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub streams: Vec<Stream>,
    /// The ringer mode the user sees (`- mode (external)`): `NORMAL`, `VIBRATE`, `SILENT`.
    pub ringer_mode: Option<String>,
    /// The focus stack, bottom first: the last entry holds focus.
    pub focus: Vec<Focus>,
}

impl State {
    pub fn stream(&self, name: &str) -> Option<&Stream> {
        self.streams.iter().find(|s| s.name == name)
    }
}

/// A stream's volume index and range, as VolumeCtrl reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Volume {
    pub index: u32,
    pub min: u32,
    pub max: u32,
}

impl Volume {
    /// The index as a level from 0 to 100 over the stream's range (`audio_volume`'s scale).
    pub fn level(&self) -> u8 {
        level(self.index, self.min, self.max)
    }
}

/// `index` in `min..=max` as 0 to 100, rounded.
pub fn level(index: u32, min: u32, max: u32) -> u8 {
    let span = max.saturating_sub(min);
    if span == 0 {
        return 0;
    }
    let at = index.clamp(min, max) - min;
    ((u64::from(at) * 100 + u64::from(span) / 2) / u64::from(span)) as u8
}

/// The index in `min..=max` nearest `level` (0 to 100; more is 100).
pub fn index_for(level: u8, min: u32, max: u32) -> u32 {
    let span = u64::from(max.saturating_sub(min));
    min + ((u64::from(level.min(100)) * span + 50) / 100) as u32
}

impl Adb {
    /// The media stream's volume.
    pub fn media_volume(&self) -> Result<Volume, String> {
        self.volume_ctrl(&format!("--stream {MUSIC} --get"))
    }

    /// Sets the media stream's volume to `level` (0 to 100, more is 100) of its range; returns the
    /// volume it then has.
    pub fn set_media_volume(&self, level: u8) -> Result<Volume, String> {
        let now = self.media_volume()?;
        let index = index_for(level, now.min, now.max);
        let after = self.volume_ctrl(&format!("--stream {MUSIC} --set {index} --get"))?;
        if after.index != index {
            return Err(format!(
                "the media volume didn't change: asked for index {index} of {}..{}, it is {} (a fixed-volume device, or a cap on the stream)",
                after.min, after.max, after.index
            ));
        }
        Ok(after)
    }

    /// Mutes (or unmutes) the media stream, then reads back whether it is muted.
    pub fn set_media_mute(&self, on: bool) -> Result<bool, String> {
        // `cmd audio` on API 33 and 34 prints nothing and exits 0 for a command it doesn't know, and
        // earlier images have no such shell command, so only API 35 on is sent the command.
        let api = self.api_level()?;
        if api < 35 {
            return Err(format!(
                "muting needs Android 15 (API 35) or later: this image is API {api}, whose `cmd audio` has no adj-mute \
                 and which has no shell command that mutes a stream"
            ));
        }
        let command = format!("cmd audio {} {MUSIC}", if on { "adj-mute" } else { "adj-unmute" });
        let (ok, text) = self.run(&["shell", &command], self.deadlines.query)?;
        if text.contains("Unknown command") || text.contains("No shell command implementation") {
            return Err(format!(
                "muting needs Android 15 (API 35) or later: this image's `cmd audio` has no adj-mute, and earlier \
                 images have no shell command that mutes a stream ({})",
                text.trim()
            ));
        }
        if !ok {
            return Err(failure(&command, &text));
        }
        let state = self.audio_state()?;
        let muted = state.stream("music").map(|s| s.muted).ok_or("`dumpsys audio` doesn't list STREAM_MUSIC")?;
        if muted != on {
            return Err(format!("`{command}` ran, but the media stream is still {}", if muted { "muted" } else { "not muted" }));
        }
        Ok(muted)
    }

    /// What `dumpsys audio` says about the streams, the ringer and focus.
    pub fn audio_state(&self) -> Result<State, String> {
        let out = self.shell("dumpsys audio", self.deadlines.query)?;
        let state = parse_dumpsys(&out);
        if state.streams.is_empty() {
            return Err(failure("dumpsys audio", "no stream volumes in its output"));
        }
        if state.stream("music").is_some_and(|m| m.max.is_none()) {
            return Err(failure("dumpsys audio", "STREAM_MUSIC has no `Max:` line"));
        }
        Ok(state)
    }

    /// VolumeCtrl with `args`: `cmd media_session volume` (API 30 on), else the `media` tool that
    /// older images have. Both print `[V] volume is <n> in range [<min>..<max>]` for `--get`.
    fn volume_ctrl(&self, args: &str) -> Result<Volume, String> {
        let mut last = String::new();
        for tool in ["cmd media_session volume", "media volume"] {
            let command = format!("{tool} {args}");
            let (_, text) = self.run(&["shell", &command], self.deadlines.query)?;
            if let Some(error) = text.lines().map(str::trim).find(|l| l.starts_with("Error:")) {
                return Err(format!("{command} failed: {error}"));
            }
            if let Some(v) = parse_volume(&text) {
                return Ok(v);
            }
            last = failure(&command, &text);
        }
        Err(last)
    }
}

/// VolumeCtrl's `[V] volume is 5 in range [0..25]`.
pub fn parse_volume(text: &str) -> Option<Volume> {
    let line = text.lines().find_map(|l| l.split_once("volume is ").map(|(_, rest)| rest))?;
    let (index, range) = line.split_once(" in range [")?;
    let (min, max) = range.trim_end().trim_end_matches(']').split_once("..")?;
    Some(Volume { index: index.trim().parse().ok()?, min: min.trim().parse().ok()?, max: max.trim().parse().ok()? })
}

/// The leading number of `s` (`Min: 1 (no permission: 0)`'s `1`).
fn number(s: &str) -> Option<u32> {
    let s = s.trim();
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok()
}

/// `dumpsys audio`'s stream volumes, ringer mode and focus stack. Its layout is AudioService's
/// (API 24 to 36 print the same lines): a `Stream volumes (device: index)` section of `- STREAM_X:`
/// blocks, `- mode (external) = …` under `Ringer mode:`, and the focus stack under `Audio Focus
/// stack entries`, one ` -- `-separated line per entry.
pub fn parse_dumpsys(text: &str) -> State {
    let mut state = State::default();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.starts_with("Stream volumes (device: index)") {
            state.streams = parse_streams(&mut lines);
        } else if trimmed.starts_with("Audio Focus stack entries") {
            while let Some(entry) = lines.next_if(|l| !l.trim().is_empty()) {
                if let Some(f) = parse_focus(entry) {
                    state.focus.push(f);
                }
            }
        } else if let Some(mode) = trimmed.strip_prefix("- mode (external) =") {
            state.ringer_mode.get_or_insert_with(|| mode.trim().to_owned());
        }
    }
    state
}

/// The `- STREAM_X:` blocks, up to the first line that is neither one of them nor inside one.
fn parse_streams<'a>(lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>) -> Vec<Stream> {
    let mut streams: Vec<Stream> = Vec::new();
    // Each stream's index per device (`Current:`), for a dump without `streamVolume:`.
    let mut current: Vec<(String, u32)> = Vec::new();
    let finish = |streams: &mut Vec<Stream>, current: &mut Vec<(String, u32)>| {
        if let Some(s) = streams.last_mut().filter(|s| s.volume.is_none()) {
            s.volume = current.iter().find(|(d, _)| s.devices.contains(d)).map(|(_, i)| *i);
        }
        current.clear();
    };
    while let Some(line) = lines.next_if(|l| l.trim().is_empty() || l.starts_with(' ') || l.starts_with("- STREAM_")) {
        let t = line.trim();
        if let Some(name) = t.strip_prefix("- STREAM_").and_then(|n| n.strip_suffix(':')) {
            // Newer images print `STREAM_SYSTEM (aliased to: STREAM_RING):`.
            let name = name.split(' ').next().unwrap_or(name);
            finish(&mut streams, &mut current);
            streams.push(Stream { name: name.to_ascii_lowercase(), muted: false, volume: None, min: 0, max: None, devices: Vec::new() });
            continue;
        }
        let Some(s) = streams.last_mut() else { continue };
        if let Some(v) = t.strip_prefix("Muted:") {
            s.muted = v.trim() == "true";
        } else if let Some(v) = t.strip_prefix("Min:") {
            s.min = number(v).unwrap_or(0);
        } else if let Some(v) = t.strip_prefix("Max:") {
            s.max = number(v);
        } else if let Some(v) = t.strip_prefix("streamVolume:") {
            s.volume = number(v);
        } else if let Some(v) = t.strip_prefix("Current:") {
            // `2 (speaker): 5, 40000000 (default): 15`
            current = v
                .split(',')
                .filter_map(|e| {
                    let (device, index) = e.rsplit_once(':')?;
                    let name = device.split_once('(').and_then(|(_, n)| n.split_once(')')).map_or(device.trim(), |(n, _)| n);
                    Some((name.to_owned(), number(index)?))
                })
                .collect();
        } else if let Some(v) = t.strip_prefix("Devices:") {
            // `speaker(2)`, `speaker(2), bt_a2dp(80)` or, older, `speaker`.
            s.devices = v.split([',', '|']).map(|d| d.split('(').next().unwrap_or(d).trim().to_owned()).filter(|d| !d.is_empty()).collect();
        }
    }
    finish(&mut streams, &mut current);
    streams
}

/// One focus stack line: `source:… -- pack: com.example.app -- client: … -- gain: GAIN -- … -- uid: 10190 -- …`.
fn parse_focus(line: &str) -> Option<Focus> {
    let field = |key: &str| line.split(" -- ").find_map(|part| part.trim().strip_prefix(key).map(|v| v.trim().to_owned()));
    Some(Focus {
        package: field("pack:")?,
        uid: field("uid:").and_then(|u| number(&u)),
        gain: field("gain:").unwrap_or_default(),
        client: field("client:").unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_and_indexes_round_over_the_range() {
        assert_eq!((index_for(0, 0, 25), index_for(60, 0, 25), index_for(100, 0, 25), index_for(255, 0, 25)), (0, 15, 25, 25));
        assert_eq!((index_for(0, 1, 5), index_for(50, 1, 5), index_for(100, 1, 5)), (1, 3, 5));
        assert_eq!((level(5, 0, 25), level(15, 0, 25), level(25, 0, 25), level(3, 1, 5)), (20, 60, 100, 50));
        assert_eq!((level(9, 0, 0), level(30, 0, 25)), (0, 100), "an empty range, an index past it");
        for l in 0..=100u8 {
            let i = index_for(l, 0, 15);
            assert!(level(i, 0, 15).abs_diff(l) <= 4, "{l} → {i}");
        }
    }

    #[test]
    fn volume_ctrl_output_parses() {
        let out = "[V] will control stream=3\n[V] will get volume\n[V] Connecting to AudioService\n[V] volume is 5 in range [0..25]\n";
        assert_eq!(parse_volume(out), Some(Volume { index: 5, min: 0, max: 25 }));
        assert_eq!(parse_volume("[V] volume is 4 in range [1..5]\r\n"), Some(Volume { index: 4, min: 1, max: 5 }));
        assert_eq!(parse_volume("No shell command implementation."), None);
        assert_eq!(parse_volume("[V] volume is x in range [0..25]"), None);
    }

    /// A dump without `Min:` or `streamVolume:`, devices without their codes.
    #[test]
    fn an_older_dump_takes_the_index_of_the_device_it_plays_on() {
        let dump = "Stream volumes (device: index)\n- STREAM_MUSIC:\n   Muted: true\n   Max: 15\n   \
                    Current: 2 (speaker): 9, 40000000 (default): 15\n   Devices: speaker\n\n- STREAM_ALARM:\n   Muted: false\n   \
                    Max: 7\n   Current: 40000000 (default): 6\n   Devices: speaker\n\n- mute affected streams = 0x2f\n";
        let state = parse_dumpsys(dump);
        assert_eq!(
            state.streams[0],
            Stream { name: "music".into(), muted: true, volume: Some(9), min: 0, max: Some(15), devices: vec!["speaker".into()] }
        );
        assert_eq!(state.streams[1].volume, None, "no index for the device it plays on");
        assert_eq!(state.streams.len(), 2, "the section ends at the first line outside a stream");
        assert_eq!((state.ringer_mode, state.focus), (None, vec![]));
    }

    #[test]
    fn aliased_stream_names_are_cut_and_a_missing_max_is_kept() {
        let dump = "Stream volumes (device: index)\n- STREAM_SYSTEM (aliased to: STREAM_RING):\n   Muted: false\n   Max: 7\n   \
                    Devices: speaker\n- STREAM_MUSIC:\n   Muted: false\n   Devices: speaker\n";
        let state = parse_dumpsys(dump);
        assert_eq!(state.streams[0].name, "system");
        assert_eq!((state.streams[0].max, state.streams[1].max), (Some(7), None));
    }

    #[test]
    fn a_dump_without_streams_is_empty() {
        assert_eq!(parse_dumpsys("Can't find service: audio\n"), State::default());
    }
}
