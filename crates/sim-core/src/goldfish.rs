//! What the Android emulator's guest (goldfish/ranchu) expects from the host, as stock decides
//! it: console key names, the keys the `qwerty2` keyboard reports, and the sensors a device
//! advertises. Shared by simee's VMM and console and by front ends.
use crate::form_factor::FormFactor;

/// The stock console's `KEY_*` names (android/emu/hardware/include/android/hw-events.h, prefix
/// dropped) with their Linux keycodes, stock's duplicates (REPLY..BATTERY) included.
const STOCK_KEYS: &str = "\
ESC=1 1=2 2=3 3=4 4=5 5=6 6=7 7=8 8=9 9=10 0=11 MINUS=12 EQUAL=13 BACKSPACE=14 \
TAB=15 Q=16 W=17 E=18 R=19 T=20 Y=21 U=22 I=23 O=24 P=25 LEFTBRACE=26 RIGHTBRACE=27 ENTER=28 \
LEFTCTRL=29 A=30 S=31 D=32 F=33 G=34 H=35 J=36 K=37 L=38 SEMICOLON=39 APOSTROPHE=40 GRAVE=41 LEFTSHIFT=42 \
BACKSLASH=43 Z=44 X=45 C=46 V=47 B=48 N=49 M=50 COMMA=51 DOT=52 SLASH=53 RIGHTSHIFT=54 KPASTERISK=55 LEFTALT=56 \
SPACE=57 CAPSLOCK=58 F1=59 F2=60 F3=61 F4=62 F5=63 F6=64 F7=65 F8=66 F9=67 F10=68 NUMLOCK=69 SCROLLLOCK=70 \
KP7=71 KP8=72 KP9=73 KPMINUS=74 KP4=75 KP5=76 KP6=77 KPPLUS=78 KP1=79 KP2=80 KP3=81 KP0=82 KPDOT=83 ZENKAKUHANKAKU=85 \
102ND=86 F11=87 F12=88 RO=89 KATAKANA=90 HIRAGANA=91 HENKAN=92 KATAKANAHIRAGANA=93 MUHENKAN=94 KPJPCOMMA=95 KPENTER=96 \
RIGHTCTRL=97 KPSLASH=98 SYSRQ=99 RIGHTALT=100 LINEFEED=101 HOME=102 UP=103 PAGEUP=104 LEFT=105 RIGHT=106 END=107 DOWN=108 \
PAGEDOWN=109 INSERT=110 DELETE=111 MACRO=112 MUTE=113 VOLUMEDOWN=114 VOLUMEUP=115 POWER=116 KPEQUAL=117 KPPLUSMINUS=118 \
PAUSE=119 KPCOMMA=121 HANGEUL=122 HANJA=123 YEN=124 LEFTMETA=125 RIGHTMETA=126 COMPOSE=127 STOP=128 AGAIN=129 PROPS=130 \
UNDO=131 FRONT=132 COPY=133 OPEN=134 PASTE=135 FIND=136 CUT=137 HELP=138 MENU=139 CALC=140 SETUP=141 SLEEP=142 WAKEUP=143 \
FILE=144 SENDFILE=145 DELETEFILE=146 XFER=147 PROG1=148 PROG2=149 WWW=150 MSDOS=151 COFFEE=152 DIRECTION=153 \
CYCLEWINDOWS=154 MAIL=155 BOOKMARKS=156 COMPUTER=157 BACK=158 FORWARD=159 CLOSECD=160 EJECTCD=161 EJECTCLOSECD=162 \
NEXTSONG=163 PLAYPAUSE=164 PREVIOUSSONG=165 STOPCD=166 RECORD=167 REWIND=168 PHONE=169 ISO=170 CONFIG=171 HOMEPAGE=172 \
REFRESH=173 EXIT=174 MOVE=175 EDIT=176 SCROLLUP=177 SCROLLDOWN=178 KPLEFTPAREN=179 KPRIGHTPAREN=180 NEW=181 REDO=182 \
F13=183 F14=184 F15=185 F16=186 F17=187 F18=188 F19=189 F20=190 F21=191 F22=192 F23=193 F24=194 PLAYCD=200 PAUSECD=201 \
PROG3=202 PROG4=203 SUSPEND=205 CLOSE=206 PLAY=207 FASTFORWARD=208 BASSBOOST=209 PRINT=210 HP=211 CAMERA=212 SOUND=213 \
QUESTION=214 EMAIL=215 CHAT=216 SEARCH=217 CONNECT=218 FINANCE=219 SPORT=220 SHOP=221 ALTERASE=222 CANCEL=223 \
BRIGHTNESSDOWN=224 BRIGHTNESSUP=225 MEDIA=226 STAR=227 SHARP=228 SOFT1=229 SOFT2=230 SEND=231 CENTER=232 HEADSETHOOK=233 \
0_5=234 2_5=235 SWITCHVIDEOMODE=236 KBDILLUMTOGGLE=237 KBDILLUMDOWN=238 KBDILLUMUP=239 REPLY=232 FORWARDMAIL=233 SAVE=234 \
DOCUMENTS=235 BATTERY=236 UNKNOWN=240 NUM=241 FOCUS=242 PLUS=243 NOTIFICATION=244 OK=0x160 SELECT=0x161 GOTO=0x162 \
CLEAR=0x163 POWER2=0x164 OPTION=0x165 INFO=0x166 TIME=0x167 VENDOR=0x168 ARCHIVE=0x169 PROGRAM=0x16a CHANNEL=0x16b \
FAVORITES=0x16c EPG=0x16d PVR=0x16e MHP=0x16f LANGUAGE=0x170 TITLE=0x171 SUBTITLE=0x172 ANGLE=0x173 ZOOM=0x174 MODE=0x175 \
KEYBOARD=0x176 SCREEN=0x177 PC=0x178 TV=0x179 TV2=0x17a VCR=0x17b VCR2=0x17c SAT=0x17d SAT2=0x17e CD=0x17f TAPE=0x180 \
RADIO=0x181 TUNER=0x182 PLAYER=0x183 TEXT=0x184 DVD=0x185 AUX=0x186 MP3=0x187 AUDIO=0x188 VIDEO=0x189 DIRECTORY=0x18a \
LIST=0x18b MEMO=0x18c CALENDAR=0x18d RED=0x18e GREEN=0x18f YELLOW=0x190 BLUE=0x191 CHANNELUP=0x192 CHANNELDOWN=0x193 \
FIRST=0x194 LAST=0x195 AB=0x196 NEXT=0x197 RESTART=0x198 SLOW=0x199 SHUFFLE=0x19a BREAK=0x19b PREVIOUS=0x19c DIGITS=0x19d \
TEEN=0x19e TWEN=0x19f DEL_EOL=0x1c0 DEL_EOS=0x1c1 INS_LINE=0x1c2 DEL_LINE=0x1c3 FN=0x1d0 FN_ESC=0x1d1 FN_F1=0x1d2 \
FN_F2=0x1d3 FN_F3=0x1d4 FN_F4=0x1d5 FN_F5=0x1d6 FN_F6=0x1d7 FN_F7=0x1d8 FN_F8=0x1d9 FN_F9=0x1da FN_F10=0x1db FN_F11=0x1dc \
FN_F12=0x1dd FN_1=0x1de FN_2=0x1df FN_D=0x1e0 FN_E=0x1e1 FN_F=0x1e2 FN_S=0x1e3 FN_B=0x1e4 BRL_DOT1=0x1f1 BRL_DOT2=0x1f2 \
BRL_DOT3=0x1f3 BRL_DOT4=0x1f4 BRL_DOT5=0x1f5 BRL_DOT6=0x1f6 BRL_DOT7=0x1f7 BRL_DOT8=0x1f8 VOICECOMMAND=0x246";

/// Names stock's console lacks: the Linux name of Android's APP_SWITCH (Recents) key.
const EXTRA_KEYS: &str = "APPSELECT=580";

/// A number as stock's console reads it (`strtol` base 0, less octal): decimal or `0x` hex.
fn number(s: &str) -> Option<u16> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u16::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// The Linux keycode for a console key, as stock's console takes it in `event send
/// EV_KEY:<key>:<value>`: a number (decimal or `0x` hex, so `2` is code 2, not the `1` key) or a
/// `KEY_*` name (here the prefix is optional and case doesn't matter).
pub fn keycode(key: &str) -> Option<u16> {
    if let Some(code) = number(key) {
        return Some(code);
    }
    let upper = key.to_ascii_uppercase();
    let name = upper.strip_prefix("KEY_").unwrap_or(&upper);
    STOCK_KEYS.split_whitespace().chain(EXTRA_KEYS.split_whitespace()).find_map(|entry| {
        let (k, code) = entry.split_once('=')?;
        if k == name { number(code) } else { None }
    })
}

/// Keys stock adds to its `qwerty2` keyboard on top of QEMU's qcode map (hw/input/
/// virtio-input-hid.c, "mandatory android keys"): HOME, BACK, SEND, END, SOFT1, VOLUMEUP,
/// VOLUMEDOWN, SOFT2, POWER, SEARCH, SLEEP, APP_SWITCH (580), STEM_PRIMARY, STEM_1..3, ASSIST
/// (581–585), MOVE_HOME, MOVE_END (452, 453), CENTER and GREEN (GRAVE in `qwerty2.kl`).
pub const ANDROID_KEYS: [u16; 21] =
    [102, 158, 231, 107, 229, 115, 114, 230, 116, 217, 142, 580, 581, 582, 583, 584, 585, 452, 453, 232, 399];

/// Whether the `qwerty2` keyboard reports `code` (the kernel drops codes a device doesn't
/// advertise): the PC keyboard (QEMU's qcode map lies within 1..=248) and [`ANDROID_KEYS`].
pub fn keyboard_has(code: u16) -> bool {
    (1..=248).contains(&code) || ANDROID_KEYS.contains(&code)
}

/// Sensor protocol names in handle order (handle = index; android/hw-sensors.h), as the goldfish
/// sensors HAL lists them.
pub const SENSORS: [&str; 18] = [
    "acceleration",
    "gyroscope",
    "magnetic",
    "orientation",
    "temperature",
    "proximity",
    "light",
    "pressure",
    "humidity",
    "magnetic-uncalibrated",
    "gyroscope-uncalibrated",
    "hinge-angle0",
    "hinge-angle1",
    "hinge-angle2",
    "heart-rate",
    "rgbc-light",
    "wrist-tilt",
    "acceleration-uncalibrated",
];

/// The handle of sensor `name`.
pub fn sensor_handle(name: &str) -> Option<usize> {
    SENSORS.iter().position(|s| *s == name)
}

/// How many values sensor `name` (a protocol name) carries, as emu's console sets them: the motion
/// and orientation sensors 3 (x, y, z), `rgbc-light` 4, the rest 1.
pub fn sensor_values(name: &str) -> Option<usize> {
    sensor_handle(name)?;
    Some(match name {
        "acceleration"
        | "acceleration-uncalibrated"
        | "gyroscope"
        | "gyroscope-uncalibrated"
        | "magnetic"
        | "magnetic-uncalibrated"
        | "orientation" => 3,
        "rgbc-light" => 4,
        _ => 1,
    })
}

/// The sensors a device advertises (the `list-sensors` bitmask), decided like stock's
/// android/hw-sensors.cpp: each `config.ini` flag with its hardware-properties.ini default, hinge
/// angles 1 and 2 by `hw.sensor.hinge.count`, and heart rate and wrist tilt forced on for Wear OS
/// at API 28 and later. Beyond stock, a foldable always gets hinge angle 0 (`fold` moves it).
pub fn sensor_mask(get: impl Fn(&str) -> Option<String>, form_factor: FormFactor, api_level: Option<u32>) -> u32 {
    let flag = |k: &str, default: bool| get(k).map_or(default, |v| matches!(v.trim(), "yes" | "true" | "1"));
    let mut mask = 0u32;
    let mut on = |name: &str| {
        if let Some(h) = sensor_handle(name) {
            mask |= 1 << h;
        }
    };
    for (key, sensor, default) in [
        ("hw.accelerometer", "acceleration", true),
        ("hw.accelerometer_uncalibrated", "acceleration-uncalibrated", true),
        ("hw.gyroscope", "gyroscope", true),
        ("hw.sensors.proximity", "proximity", true),
        ("hw.sensors.magnetic_field", "magnetic", true),
        ("hw.sensors.magnetic_field_uncalibrated", "magnetic-uncalibrated", true),
        ("hw.sensors.gyroscope_uncalibrated", "gyroscope-uncalibrated", true),
        ("hw.sensors.orientation", "orientation", true),
        ("hw.sensors.temperature", "temperature", true),
        ("hw.sensors.light", "light", true),
        ("hw.sensors.pressure", "pressure", true),
        ("hw.sensors.humidity", "humidity", true),
        ("hw.sensors.rgbclight", "rgbc-light", false),
    ] {
        if flag(key, default) {
            on(sensor);
        }
    }
    if flag("hw.sensor.hinge", false) || form_factor == FormFactor::Foldable {
        on("hinge-angle0");
    }
    if flag("hw.sensor.hinge", false) {
        let count: u32 = get("hw.sensor.hinge.count").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        if count >= 2 {
            on("hinge-angle1");
        }
        if count >= 3 {
            on("hinge-angle2");
        }
    }
    let wear = form_factor == FormFactor::Watch && api_level.is_some_and(|a| a >= 28);
    if flag("hw.sensors.heart_rate", false) || wear {
        on("heart-rate");
    }
    if flag("hw.sensors.wrist_tilt", false) || wear {
        on("wrist-tilt");
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    fn bit(name: &str) -> u32 {
        1 << sensor_handle(name).expect("sensor")
    }

    #[test]
    fn key_names_and_numbers() {
        assert_eq!(keycode("KEY_POWER"), Some(116));
        assert_eq!(keycode("a"), Some(30));
        assert_eq!(keycode("KEY_Q"), Some(16));
        assert_eq!(keycode("KEY_1"), Some(2));
        assert_eq!(keycode("key_home"), Some(102));
        assert_eq!(keycode("nope"), None);
        assert_eq!(keycode(""), None);
        // Numbers are codes, decimal or hex, like stock's strtol.
        assert_eq!(keycode("2"), Some(2), "a number is a code, not the `2` key");
        assert_eq!(keycode("0x161"), Some(353));
        assert_eq!(keycode("580"), Some(580));
        // Stock's TV, Wear and media names.
        for (name, code) in [
            ("KEY_CENTER", 232),
            ("KEY_SELECT", 0x161),
            ("KEY_OK", 0x160),
            ("KEY_PROGRAM", 0x16a),
            ("KEY_EPG", 0x16d),
            ("KEY_TV", 0x179),
            ("KEY_HOMEPAGE", 172),
            ("KEY_VOICECOMMAND", 0x246),
            ("KEY_PLAYPAUSE", 164),
            ("KEY_SOFT1", 229),
            ("KEY_SEND", 231),
            ("KEY_GREEN", 0x18f),
            ("KEY_CHANNELUP", 0x192),
            ("KEY_FASTFORWARD", 208),
            ("KEY_REPLY", 232),
            ("KEY_102ND", 86),
            ("KEY_APPSELECT", 580),
        ] {
            assert_eq!(keycode(name), Some(code), "{name}");
        }
        assert_eq!(STOCK_KEYS.split_whitespace().count(), 339, "every KEY_CODE in stock's hw-events.h");
        assert!(STOCK_KEYS.split_whitespace().all(|e| e.split_once('=').and_then(|(_, c)| number(c)).is_some()));
    }

    #[test]
    fn the_keyboard_reports_what_qwerty2_maps() {
        // HOME, BACK, MENU (SOFT1), PLAYPAUSE, REWIND, FASTFORWARD, CENTER, GREEN, MOVE_HOME/END,
        // APP_SWITCH, STEM_PRIMARY..ASSIST.
        for code in [102, 158, 164, 168, 208, 229, 230, 231, 232, 399, 452, 453, 580, 581, 582, 583, 584, 585] {
            assert!(keyboard_has(code), "{code}");
        }
        assert!(!keyboard_has(0), "KEY_RESERVED");
        assert!(!keyboard_has(0x161), "KEY_SELECT isn't a stock keyboard key");
        assert!(!keyboard_has(0x130), "no gamepad buttons (they'd make it a gamepad)");
    }

    #[test]
    fn every_button_and_typed_key_reaches_the_keyboard() {
        for b in crate::hid::Button::ALL {
            if let Some(key) = b.android_key() {
                let code = keycode(key).unwrap_or_else(|| panic!("{b:?}: {key} has no keycode"));
                assert!(keyboard_has(code), "{b:?}: the keyboard doesn't report {code}, so the kernel would drop it");
            }
        }
        for usage in 0..=0xff {
            if let Some(code) = crate::hid::linux_keycode(usage) {
                assert!(keyboard_has(code), "HID usage {usage:#x} types {code}, which the keyboard doesn't report");
            }
        }
    }

    #[test]
    fn sensors_follow_the_config_like_stock() {
        let phone = sensor_mask(config(&[]), FormFactor::Phone, Some(36));
        assert_eq!(phone, 0x7ff | bit("acceleration-uncalibrated"), "stock defaults: motion and environment");
        assert_eq!(phone & (bit("hinge-angle0") | bit("heart-rate") | bit("wrist-tilt") | bit("rgbc-light")), 0);

        // Wear OS at API 28+ gets heart rate and wrist tilt; older Wear doesn't.
        let watch = sensor_mask(config(&[]), FormFactor::Watch, Some(34));
        assert_eq!(watch, phone | bit("heart-rate") | bit("wrist-tilt"));
        assert_eq!(sensor_mask(config(&[]), FormFactor::Watch, Some(26)), phone);
        assert_eq!(sensor_mask(config(&[]), FormFactor::Watch, None), phone);
        assert_eq!(sensor_mask(config(&[("hw.sensors.heart_rate", "yes")]), FormFactor::Phone, Some(36)), phone | bit("heart-rate"));

        // A foldable's hinge, with more angles by count; a foldable by name still gets angle 0.
        let fold = sensor_mask(config(&[("hw.sensor.hinge", "yes"), ("hw.sensor.hinge.count", "1")]), FormFactor::Foldable, Some(36));
        assert_eq!(fold, phone | bit("hinge-angle0"));
        let three = sensor_mask(config(&[("hw.sensor.hinge", "yes"), ("hw.sensor.hinge.count", "3")]), FormFactor::Foldable, Some(36));
        assert_eq!(three, phone | bit("hinge-angle0") | bit("hinge-angle1") | bit("hinge-angle2"));
        assert_eq!(sensor_mask(config(&[]), FormFactor::Foldable, Some(36)), phone | bit("hinge-angle0"));

        // Automotive's config.ini turns most sensors off.
        let car = config(&[
            ("hw.sensors.light", "no"),
            ("hw.sensors.pressure", "no"),
            ("hw.sensors.humidity", "no"),
            ("hw.sensors.proximity", "no"),
            ("hw.sensors.magnetic_field", "no"),
            ("hw.sensors.orientation", "no"),
            ("hw.sensors.temperature", "no"),
            ("hw.sensor.hinge", "no"),
        ]);
        let expect = ["acceleration", "acceleration-uncalibrated", "gyroscope", "magnetic-uncalibrated", "gyroscope-uncalibrated"];
        assert_eq!(sensor_mask(car, FormFactor::Auto, Some(34)), expect.iter().map(|n| bit(n)).sum::<u32>());
    }
}
