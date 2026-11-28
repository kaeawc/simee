//! Input from the viewer over a WebRTC data channel: the `simee-input` protocol.
//!
//! The publisher offers a reliable, ordered data channel labelled [`LABEL`] (and accepts one the
//! peer opens with that label). Each message is one JSON object in a text frame, versioned with
//! `"v"`; the viewer sends input, the publisher answers with `hello`, `ack`, `pong` and `error`.
//! The wire format is in `docs/streaming.md` ("Input over a data channel").
//!
//! - [`decode`] and [`Message::to_json`]: viewer messages (touch with pointer ids, keys as USB HID
//!   usages or named keys, text, hardware buttons, rotation, fold posture, wheel, ping).
//! - [`Reply`]: what goes back, with [`Reply::to_json`] and [`Reply::decode`].
//! - [`InputSession`]: one channel's state: the rate limit, pointer bookkeeping, move coalescing,
//!   and acks with the viewer's timestamp echoed beside the server's receive time, so
//!   input-to-photon latency can be measured. Every malformed or refused message becomes an
//!   `error` reply; nothing a viewer sends can panic it.
//! - [`InputSink`]: where the events go (a device's input path; wiring it to simee's devices is
//!   #266). [`DisplayMap`] maps the stream-frame coordinates to device pixels.
//!
//! Coordinates are normalised to the stream frame (`0..=1` on each axis, origin top left) and
//! carry the frame size the viewer saw (`w`, `h`, the decoded video's size), so a sink can map
//! them onto the device whatever the encoder's downscale, and refuse a point from a frame of a
//! different shape (the device rotated while the input was in flight).

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Write as _};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The data channel's label.
pub const LABEL: &str = "simee-input";
/// The protocol version this build speaks.
pub const VERSION: u32 = 1;

/// The largest frame side a message may claim, in pixels.
const MAX_FRAME_SIDE: u32 = 16_384;
/// The largest wheel delta, in pixels of the viewer's frame.
const MAX_WHEEL_DELTA: f64 = 100_000.0;
/// JSON nesting allowed in a message (they are flat objects).
const MAX_DEPTH: usize = 4;
/// The largest integer a JSON number carries exactly.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// A frame size in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameSize {
    pub width: u32,
    pub height: u32,
}

impl FrameSize {
    pub const fn new(width: u32, height: u32) -> FrameSize {
        FrameSize { width, height }
    }
}

impl fmt::Display for FrameSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// A point in the stream frame: `x` and `y` in `0..=1` (top left origin), and the size of the
/// frame the viewer saw it on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
    pub frame: FrameSize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    /// The viewer lost the pointer (it left the video, the page lost focus): lift without a tap.
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyPhase {
    Down,
    Up,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ButtonPhase {
    Down,
    Up,
    /// Down then up.
    Press,
}

/// A hardware button. Which ones a device has is the sink's business: it refuses the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    Home,
    Power,
    VolumeUp,
    VolumeDown,
    Back,
    AppSwitch,
    Menu,
    Assistant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Orientation {
    Portrait,
    LandscapeLeft,
    LandscapeRight,
    PortraitUpsideDown,
}

/// A foldable's posture, as the Android emulator names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Posture {
    Closed,
    HalfOpen,
    Open,
    Flipped,
    Tent,
}

/// One input event, validated, as a sink receives it.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// A touch contact; `pointer` tells simultaneous contacts apart (multi-touch).
    Touch {
        pointer: u32,
        phase: TouchPhase,
        point: Point,
    },
    /// A keyboard key as a USB HID usage on the keyboard page (0x07): `0x04` is A, `0x28` Enter.
    Key {
        phase: KeyPhase,
        usage: u16,
    },
    /// Text to type as it is (an IME commit, a paste).
    Text(String),
    Button {
        button: Button,
        phase: ButtonPhase,
    },
    Rotate(Orientation),
    Fold(Posture),
    /// A scroll at `point`, by `dx`/`dy` pixels of the frame the viewer saw (positive is right
    /// and down, as a browser's `WheelEvent` in pixel mode).
    Wheel {
        point: Point,
        dx: f64,
        dy: f64,
    },
}

/// What a viewer message asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    /// A touch; `point` may be left out on `up` and `cancel`, which then lift where the pointer
    /// last was.
    Touch { pointer: u32, phase: TouchPhase, point: Option<Point> },
    /// Any other input event.
    Input(InputEvent),
    /// Answered with a `pong` right away, never queued behind input.
    Ping,
}

/// One message from the viewer.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// The viewer's sequence number, echoed in the reply.
    pub seq: Option<u64>,
    /// The viewer's timestamp in milliseconds (any clock: `performance.now()`), echoed in the
    /// reply. An input message that carries one is acknowledged once the sink took it.
    pub t: Option<f64>,
    pub body: Body,
}

/// Why a message was refused: the `code` of an `error` reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// A binary frame, or text that isn't UTF-8.
    Encoding,
    /// Not a JSON object.
    Json,
    /// Larger than [`InputConfig::max_message_bytes`].
    Size,
    /// No `v`, or a version this build doesn't speak.
    Version,
    /// An unknown `kind`.
    Kind,
    /// A missing or out-of-range field.
    Field,
    /// A move or lift for a pointer that isn't down, a second down, or too many pointers.
    Pointer,
    /// Over the rate limit: the message was dropped.
    Rate,
    /// The sink refused the event (a button the device lacks, a stale frame size).
    Sink,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Encoding => "encoding",
            ErrorCode::Json => "json",
            ErrorCode::Size => "size",
            ErrorCode::Version => "version",
            ErrorCode::Kind => "kind",
            ErrorCode::Field => "field",
            ErrorCode::Pointer => "pointer",
            ErrorCode::Rate => "rate",
            ErrorCode::Sink => "sink",
        }
    }

    fn parse(s: &str) -> Option<ErrorCode> {
        const ALL: [ErrorCode; 9] = [
            ErrorCode::Encoding,
            ErrorCode::Json,
            ErrorCode::Size,
            ErrorCode::Version,
            ErrorCode::Kind,
            ErrorCode::Field,
            ErrorCode::Pointer,
            ErrorCode::Rate,
            ErrorCode::Sink,
        ];
        ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// A refused message: the code, a message for a person, and the `seq` when it could be read.
#[derive(Clone, Debug, PartialEq)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
    pub seq: Option<u64>,
}

impl ProtocolError {
    fn new(code: ErrorCode, message: impl Into<String>) -> ProtocolError {
        ProtocolError { code, message: message.into(), seq: None }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ProtocolError {}

/// What the publisher sends back on the channel.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    /// Sent once when the channel opens: the versions this build speaks.
    Hello {
        versions: Vec<u32>,
    },
    /// An input message with a `t` was handed to the sink. `rx` is when it arrived and `done` when
    /// the sink returned, both in Unix milliseconds on the server's clock.
    Ack {
        seq: Option<u64>,
        t: Option<f64>,
        rx: f64,
        done: f64,
    },
    /// The answer to a `ping`, with its arrival time (Unix milliseconds).
    Pong {
        seq: Option<u64>,
        t: Option<f64>,
        rx: f64,
    },
    Error {
        seq: Option<u64>,
        code: ErrorCode,
        message: String,
    },
}

impl Reply {
    fn error(e: ProtocolError) -> Reply {
        Reply::Error { seq: e.seq, code: e.code, message: e.message }
    }

    /// The compact JSON for the wire.
    pub fn to_json(&self) -> String {
        let mut w = JsonWriter::new();
        w.num("v", f64::from(VERSION));
        match self {
            Reply::Hello { versions } => {
                w.str("kind", "hello");
                w.str("protocol", LABEL);
                w.raw("versions", &format!("[{}]", versions.iter().map(u32::to_string).collect::<Vec<_>>().join(",")));
            }
            Reply::Ack { seq, t, rx, done } => {
                w.str("kind", "ack");
                w.opt_num("seq", seq.map(|s| s as f64));
                w.opt_num("t", *t);
                w.num("rx", *rx);
                w.num("done", *done);
            }
            Reply::Pong { seq, t, rx } => {
                w.str("kind", "pong");
                w.opt_num("seq", seq.map(|s| s as f64));
                w.opt_num("t", *t);
                w.num("rx", *rx);
            }
            Reply::Error { seq, code, message } => {
                w.str("kind", "error");
                w.opt_num("seq", seq.map(|s| s as f64));
                w.str("code", code.as_str());
                w.str("message", message);
            }
        }
        w.finish()
    }

    /// Parse a reply (what a viewer, or a test, does with the publisher's messages).
    pub fn decode(text: &str) -> Result<Reply, ProtocolError> {
        let obj = Obj::parse(text)?;
        obj.version()?;
        let seq = obj.opt_u64("seq")?;
        let t = obj.opt_f64("t")?;
        match obj.req_str("kind")? {
            "hello" => {
                let Some(Value::Arr(items)) = obj.get("versions") else {
                    return Err(ProtocolError::new(ErrorCode::Field, "`versions` must be an array"));
                };
                let versions = items
                    .iter()
                    .map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()))
                    .collect::<Option<Vec<u32>>>()
                    .ok_or_else(|| ProtocolError::new(ErrorCode::Field, "`versions` must hold integers"))?;
                Ok(Reply::Hello { versions })
            }
            "ack" => Ok(Reply::Ack { seq, t, rx: obj.req_f64("rx")?, done: obj.req_f64("done")? }),
            "pong" => Ok(Reply::Pong { seq, t, rx: obj.req_f64("rx")? }),
            "error" => {
                let code = obj.req_str("code")?;
                let code = ErrorCode::parse(code).ok_or_else(|| ProtocolError::new(ErrorCode::Field, format!("unknown code {code:?}")))?;
                Ok(Reply::Error { seq, code, message: obj.req_str("message")?.to_owned() })
            }
            other => Err(ProtocolError::new(ErrorCode::Kind, format!("unknown reply kind {other:?}"))),
        }
    }
}

/// Parse one viewer message. Unknown fields are ignored (a later minor addition stays readable);
/// an unknown version or kind, a missing field or an out-of-range value is an error.
pub fn decode(text: &str) -> Result<Message, ProtocolError> {
    let obj = Obj::parse(text)?;
    // The seq first, so even a refused message's error names it.
    let seq = obj.opt_u64("seq").ok().flatten();
    decode_obj(&obj, seq).map_err(|mut e| {
        e.seq = seq;
        e
    })
}

fn decode_obj(obj: &Obj, seq: Option<u64>) -> Result<Message, ProtocolError> {
    obj.version()?;
    let seq = match seq {
        Some(s) => Some(s),
        None => obj.opt_u64("seq")?,
    };
    let t = obj.opt_f64("t")?;
    let kind = obj.req_str("kind")?;
    let body = match kind {
        "ping" => Body::Ping,
        "touch" => {
            let phase = match obj.req_str("phase")? {
                "down" => TouchPhase::Down,
                "move" => TouchPhase::Move,
                "up" => TouchPhase::Up,
                "cancel" => TouchPhase::Cancel,
                other => return Err(field(format!("unknown touch phase {other:?}"))),
            };
            let pointer = obj.req_u64("id")?;
            let pointer = u32::try_from(pointer).map_err(|_| field(format!("pointer id {pointer} is out of range")))?;
            let point = match phase {
                TouchPhase::Down | TouchPhase::Move => Some(obj.point()?),
                TouchPhase::Up | TouchPhase::Cancel if obj.get("x").is_none() && obj.get("y").is_none() => None,
                TouchPhase::Up | TouchPhase::Cancel => Some(obj.point()?),
            };
            Body::Touch { pointer, phase, point }
        }
        "key" => {
            let phase = match obj.req_str("phase")? {
                "down" => KeyPhase::Down,
                "up" => KeyPhase::Up,
                other => return Err(field(format!("unknown key phase {other:?}"))),
            };
            let usage = match (obj.get("usage"), obj.get("key")) {
                (Some(_), Some(_)) => return Err(field("give `usage` or `key`, not both")),
                (Some(_), None) => {
                    let usage = obj.req_u64("usage")?;
                    u16::try_from(usage)
                        .ok()
                        .filter(|u| (0x04..=0xe7).contains(u))
                        .ok_or_else(|| field(format!("HID usage {usage:#x} is not a keyboard key (0x04..=0xe7)")))?
                }
                (None, Some(_)) => {
                    let name = obj.req_str("key")?;
                    named_key_usage(name).ok_or_else(|| field(format!("unknown key {name:?}")))?
                }
                (None, None) => return Err(field("a key needs `usage` or `key`")),
            };
            Body::Input(InputEvent::Key { phase, usage })
        }
        "text" => {
            let text = obj.req_str("text")?;
            if text.is_empty() {
                return Err(field("`text` is empty"));
            }
            Body::Input(InputEvent::Text(text.to_owned()))
        }
        "button" => {
            let name = obj.req_str("button")?;
            let button =
                BUTTONS.iter().find(|(n, _)| *n == name).map(|(_, b)| *b).ok_or_else(|| field(format!("unknown button {name:?}")))?;
            let phase = match obj.opt_str("phase")?.unwrap_or("press") {
                "down" => ButtonPhase::Down,
                "up" => ButtonPhase::Up,
                "press" => ButtonPhase::Press,
                other => return Err(field(format!("unknown button phase {other:?}"))),
            };
            Body::Input(InputEvent::Button { button, phase })
        }
        "rotate" => {
            let name = obj.req_str("orientation")?;
            let o = ORIENTATIONS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, o)| *o)
                .ok_or_else(|| field(format!("unknown orientation {name:?}")))?;
            Body::Input(InputEvent::Rotate(o))
        }
        "fold" => {
            let name = obj.req_str("posture")?;
            let p = POSTURES.iter().find(|(n, _)| *n == name).map(|(_, p)| *p).ok_or_else(|| field(format!("unknown posture {name:?}")))?;
            Body::Input(InputEvent::Fold(p))
        }
        "wheel" => {
            let point = obj.point()?;
            let delta = |k: &str| -> Result<f64, ProtocolError> {
                let d = obj.opt_f64(k)?.unwrap_or(0.0);
                if d.abs() > MAX_WHEEL_DELTA {
                    return Err(field(format!("`{k}` {d} is out of range")));
                }
                Ok(d)
            };
            let (dx, dy) = (delta("dx")?, delta("dy")?);
            Body::Input(InputEvent::Wheel { point, dx, dy })
        }
        other => return Err(ProtocolError::new(ErrorCode::Kind, format!("unknown kind {other:?}"))),
    };
    Ok(Message { seq, t, body })
}

fn field(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ErrorCode::Field, message)
}

const BUTTONS: [(&str, Button); 8] = [
    ("home", Button::Home),
    ("power", Button::Power),
    ("volume_up", Button::VolumeUp),
    ("volume_down", Button::VolumeDown),
    ("back", Button::Back),
    ("app_switch", Button::AppSwitch),
    ("menu", Button::Menu),
    ("assistant", Button::Assistant),
];

const ORIENTATIONS: [(&str, Orientation); 4] = [
    ("portrait", Orientation::Portrait),
    ("landscape_left", Orientation::LandscapeLeft),
    ("landscape_right", Orientation::LandscapeRight),
    ("portrait_upside_down", Orientation::PortraitUpsideDown),
];

const POSTURES: [(&str, Posture); 5] = [
    ("closed", Posture::Closed),
    ("half_open", Posture::HalfOpen),
    ("open", Posture::Open),
    ("flipped", Posture::Flipped),
    ("tent", Posture::Tent),
];

fn name_of<T: PartialEq + Copy, const N: usize>(table: &[(&'static str, T); N], v: T) -> &'static str {
    table.iter().find(|(_, x)| *x == v).map_or("", |(n, _)| n)
}

/// The keyboard-page HID usage of a key named as a browser names it: a `KeyboardEvent.code`
/// (`KeyA`, `Digit1`, `ArrowLeft`, `ShiftLeft`, `F5`) or a `KeyboardEvent.key` for the keys that
/// don't depend on the layout (`Enter`, `Escape`, `Backspace`, `Tab`, `" "`, `Shift`, ...).
pub fn named_key_usage(name: &str) -> Option<u16> {
    if let Some(letter) = name.strip_prefix("Key")
        && let [c @ b'A'..=b'Z'] = letter.as_bytes()
    {
        return Some(0x04 + u16::from(c - b'A'));
    }
    if let Some(digit) = name.strip_prefix("Digit")
        && let [c @ b'0'..=b'9'] = digit.as_bytes()
    {
        return Some(if *c == b'0' { 0x27 } else { 0x1e + u16::from(c - b'1') });
    }
    if let Some(n) = name.strip_prefix('F')
        && let Ok(n @ 1..=12) = n.parse::<u16>()
    {
        return Some(0x3a + n - 1);
    }
    Some(match name {
        "Enter" | "NumpadEnter" => 0x28,
        "Escape" => 0x29,
        "Backspace" => 0x2a,
        "Tab" => 0x2b,
        "Space" | " " => 0x2c,
        "Minus" => 0x2d,
        "Equal" => 0x2e,
        "BracketLeft" => 0x2f,
        "BracketRight" => 0x30,
        "Backslash" => 0x31,
        "Semicolon" => 0x33,
        "Quote" => 0x34,
        "Backquote" => 0x35,
        "Comma" => 0x36,
        "Period" => 0x37,
        "Slash" => 0x38,
        "CapsLock" => 0x39,
        "Insert" => 0x49,
        "Home" => 0x4a,
        "PageUp" => 0x4b,
        "Delete" => 0x4c,
        "End" => 0x4d,
        "PageDown" => 0x4e,
        "ArrowRight" => 0x4f,
        "ArrowLeft" => 0x50,
        "ArrowDown" => 0x51,
        "ArrowUp" => 0x52,
        "Control" | "ControlLeft" => 0xe0,
        "Shift" | "ShiftLeft" => 0xe1,
        "Alt" | "AltLeft" => 0xe2,
        "Meta" | "MetaLeft" => 0xe3,
        "ControlRight" => 0xe4,
        "ShiftRight" => 0xe5,
        "AltRight" => 0xe6,
        "MetaRight" => 0xe7,
        _ => return None,
    })
}

impl Message {
    /// The compact JSON a viewer sends for this message.
    pub fn to_json(&self) -> String {
        let mut w = JsonWriter::new();
        w.num("v", f64::from(VERSION));
        w.opt_num("seq", self.seq.map(|s| s as f64));
        w.opt_num("t", self.t);
        let point = |w: &mut JsonWriter, p: &Point| {
            w.num("x", p.x);
            w.num("y", p.y);
            w.num("w", f64::from(p.frame.width));
            w.num("h", f64::from(p.frame.height));
        };
        let touch = |w: &mut JsonWriter, pointer: u32, phase: TouchPhase, p: Option<&Point>| {
            w.str("kind", "touch");
            w.str(
                "phase",
                match phase {
                    TouchPhase::Down => "down",
                    TouchPhase::Move => "move",
                    TouchPhase::Up => "up",
                    TouchPhase::Cancel => "cancel",
                },
            );
            w.num("id", f64::from(pointer));
            if let Some(p) = p {
                point(w, p);
            }
        };
        match &self.body {
            Body::Ping => w.str("kind", "ping"),
            Body::Touch { pointer, phase, point } => touch(&mut w, *pointer, *phase, point.as_ref()),
            Body::Input(event) => match event {
                InputEvent::Touch { pointer, phase, point } => touch(&mut w, *pointer, *phase, Some(point)),
                InputEvent::Key { phase, usage } => {
                    w.str("kind", "key");
                    w.str("phase", if *phase == KeyPhase::Down { "down" } else { "up" });
                    w.num("usage", f64::from(*usage));
                }
                InputEvent::Text(text) => {
                    w.str("kind", "text");
                    w.str("text", text);
                }
                InputEvent::Button { button, phase } => {
                    w.str("kind", "button");
                    w.str("button", name_of(&BUTTONS, *button));
                    w.str(
                        "phase",
                        match phase {
                            ButtonPhase::Down => "down",
                            ButtonPhase::Up => "up",
                            ButtonPhase::Press => "press",
                        },
                    );
                }
                InputEvent::Rotate(o) => {
                    w.str("kind", "rotate");
                    w.str("orientation", name_of(&ORIENTATIONS, *o));
                }
                InputEvent::Fold(p) => {
                    w.str("kind", "fold");
                    w.str("posture", name_of(&POSTURES, *p));
                }
                InputEvent::Wheel { point: p, dx, dy } => {
                    w.str("kind", "wheel");
                    point(&mut w, p);
                    w.num("dx", *dx);
                    w.num("dy", *dy);
                }
            },
        }
        w.finish()
    }
}

/// Maps stream-frame points to device pixels. `device` is the size of the frames the encoder
/// takes (the framebuffer), in the coordinate space the device's input path uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayMap {
    pub device: FrameSize,
}

/// A point from a frame whose shape doesn't match the device's: the device rotated, or resized,
/// after the viewer saw the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaleFrame {
    pub seen: FrameSize,
    pub device: FrameSize,
}

impl fmt::Display for StaleFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the viewer saw a {} frame but the display is {}: its shape changed", self.seen, self.device)
    }
}

impl std::error::Error for StaleFrame {}

impl DisplayMap {
    pub const fn new(width: u32, height: u32) -> DisplayMap {
        DisplayMap { device: FrameSize::new(width, height) }
    }

    /// The size the encoder scales the device's frames to with its longest side capped at
    /// `max_side`, keeping the aspect ratio, even sides, at least 16: the rule of `simee-encode`'s
    /// `output_size`, so a test can predict the frame a viewer sees.
    pub fn stream_size(&self, max_side: Option<u32>) -> FrameSize {
        let FrameSize { width, height } = self.device;
        let long = width.max(height);
        let scale = match max_side {
            Some(max) if long > max => f64::from(max) / f64::from(long),
            _ => 1.0,
        };
        let fit = |side: u32| (((f64::from(side) * scale) as u32) & !1).max(16);
        FrameSize::new(fit(width), fit(height))
    }

    /// Whether a frame the viewer saw has the device's shape, within the encoder's rounding: each
    /// side is floored to an even number and raised to at least 16, so the scale the frame implies
    /// is a range, and the other side must fall in the range that scale allows (plus a pixel and
    /// half a percent for a browser's rounding). Extreme aspects like 4000x20 streamed as 100x16
    /// match; a rotated frame does not. A square display looks the same in every rotation, so
    /// rotating one cannot be detected from the frame's shape.
    pub fn matches(&self, seen: FrameSize) -> bool {
        if self.device.width == 0 || self.device.height == 0 || seen.width == 0 || seen.height == 0 {
            return false;
        }
        let (dev_long, dev_short, seen_long, seen_short) = if self.device.width >= self.device.height {
            (self.device.width, self.device.height, seen.width, seen.height)
        } else {
            (self.device.height, self.device.width, seen.height, seen.width)
        };
        // The unrounded size behind a seen side: [side, side + 2), or anything under 18 at the floor.
        let range = |side: u32| {
            let side = f64::from(side);
            let slack = 1.0 + 0.005 * side;
            ((if side <= 16.0 { 0.0 } else { side - slack }), side + 2.0 + slack)
        };
        let (long_lo, long_hi) = range(seen_long);
        let (short_lo, short_hi) = range(seen_short);
        let ratio = f64::from(dev_short) / f64::from(dev_long);
        // The short side the long side's range implies, against the short side's own range.
        let (implied_lo, implied_hi) = (long_lo * ratio, long_hi * ratio);
        implied_lo <= short_hi && short_lo <= implied_hi
    }

    /// The device pixel under a stream-frame point, clamped inside the display. A point from a
    /// frame of another shape is refused rather than stretched.
    pub fn to_device(&self, point: &Point) -> Result<(f64, f64), StaleFrame> {
        if !self.matches(point.frame) {
            return Err(StaleFrame { seen: point.frame, device: self.device });
        }
        let along = |n: f64, side: u32| {
            let side = f64::from(side);
            (n * side).clamp(0.0, (side - 1.0).max(0.0))
        };
        Ok((along(point.x, self.device.width), along(point.y, self.device.height)))
    }

    /// A distance in pixels of the frame the viewer saw (a wheel delta), in device pixels.
    pub fn delta_to_device(&self, dx: f64, dy: f64, seen: FrameSize) -> (f64, f64) {
        let scale = |d: f64, device: u32, seen: u32| if seen == 0 { 0.0 } else { d * f64::from(device) / f64::from(seen) };
        (scale(dx, self.device.width, seen.width), scale(dy, self.device.height, seen.height))
    }
}

/// Where a session's input goes: a device's input path. It runs on the publisher's media thread,
/// so keep it short (queue to the device and return). An `Err` goes back to the viewer as an
/// `error` reply with code `sink`; the session carries on.
///
/// `InputEvent::Text` arrives raw, exactly as the viewer sent it (control characters and newlines
/// included): a sink must never pass it to a shell or interpolate it into a command line.
pub trait InputSink: Send {
    fn input(&mut self, event: &InputEvent) -> Result<(), String>;
}

impl<F: FnMut(&InputEvent) -> Result<(), String> + Send> InputSink for F {
    fn input(&mut self, event: &InputEvent) -> Result<(), String> {
        self(event)
    }
}

/// An [`InputSession`]'s limits.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputConfig {
    /// At most one move per pointer per interval reaches the sink; moves inside it are coalesced
    /// into the latest, delivered when the interval ends or before the next non-move event.
    pub min_move_interval: Duration,
    /// The sustained message rate (token bucket refill per second); messages beyond it are
    /// dropped with a `rate` error (at most one such error per second).
    pub max_messages_per_second: u32,
    /// The bucket's size: how many messages may arrive at once.
    pub burst: u32,
    /// The largest message, in bytes.
    pub max_message_bytes: usize,
    /// How many pointers may be down at once.
    pub max_pointers: usize,
}

impl Default for InputConfig {
    fn default() -> InputConfig {
        InputConfig {
            min_move_interval: Duration::from_millis(8),
            max_messages_per_second: 1000,
            burst: 200,
            max_message_bytes: 4096,
            max_pointers: 10,
        }
    }
}

/// A move waiting for its pointer's interval to end.
#[derive(Debug)]
struct PendingMove {
    pointer: u32,
    point: Point,
    seq: Option<u64>,
    t: Option<f64>,
    rx: f64,
}

/// One input channel's state. Feed it every message ([`receive`](Self::receive)) and call
/// [`poll`](Self::poll) by [`next_due`](Self::next_due); send what they push to `out` back on the
/// channel, in order.
pub struct InputSession {
    sink: Box<dyn InputSink>,
    config: InputConfig,
    /// Each pointer that is down and where it last was.
    pointers: HashMap<u32, Point>,
    /// Keys (HID usages) and buttons that are held down, to lift on release.
    keys: HashSet<u16>,
    buttons: HashSet<Button>,
    /// When each pointer's last move reached the sink.
    last_move: HashMap<u32, Instant>,
    /// At most one per pointer, in arrival order.
    pending: Vec<PendingMove>,
    tokens: f64,
    refilled: Option<Instant>,
    rate_error: Option<Instant>,
}

impl Drop for InputSession {
    fn drop(&mut self) {
        self.release_all();
    }
}

impl fmt::Debug for InputSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputSession")
            .field("config", &self.config)
            .field("pointers", &self.pointers.len())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl InputSession {
    pub fn new(sink: impl InputSink + 'static, config: InputConfig) -> InputSession {
        InputSession {
            sink: Box::new(sink),
            config,
            pointers: HashMap::new(),
            keys: HashSet::new(),
            buttons: HashSet::new(),
            last_move: HashMap::new(),
            pending: Vec::new(),
            tokens: f64::from(config.burst),
            refilled: None,
            rate_error: None,
        }
    }

    /// The message to send when the channel opens.
    pub fn hello(&self) -> Reply {
        Reply::Hello { versions: vec![VERSION] }
    }

    /// Handle one channel message received at `now`.
    pub fn receive(&mut self, now: Instant, binary: bool, data: &[u8], out: &mut Vec<Reply>) {
        let rx = wall_ms();
        let admitted = self.take_token(now);
        // Decoding is cheap (the size cap comes first), so a lift can be recognised over budget.
        match Self::parse(self.config.max_message_bytes, binary, data) {
            Ok(message) if admitted || self.is_release(&message) => self.handle(now, rx, message, out),
            Ok(message) => self.rate_refusal(now, message.seq, out),
            Err(e) if admitted => out.push(Reply::error(e)),
            Err(_) => self.rate_refusal(now, None, out),
        }
    }

    fn parse(max_bytes: usize, binary: bool, data: &[u8]) -> Result<Message, ProtocolError> {
        if data.len() > max_bytes {
            return Err(ProtocolError::new(ErrorCode::Size, format!("{} bytes is over the {max_bytes}-byte limit", data.len())));
        }
        if binary {
            return Err(ProtocolError::new(ErrorCode::Encoding, "binary frames are not part of the protocol: send text"));
        }
        let text = std::str::from_utf8(data).map_err(|_| ProtocolError::new(ErrorCode::Encoding, "not UTF-8"))?;
        decode(text)
    }

    fn rate_refusal(&mut self, now: Instant, seq: Option<u64>, out: &mut Vec<Reply>) {
        if self.rate_error.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1)) {
            self.rate_error = Some(now);
            out.push(Reply::Error {
                seq,
                code: ErrorCode::Rate,
                message: format!("over {} messages per second: dropped", self.config.max_messages_per_second),
            });
        }
    }

    /// Whether `message` lifts something that is down (a pointer, a key, a button): never dropped
    /// by the rate limit, so a flood can't leave input stuck.
    fn is_release(&self, message: &Message) -> bool {
        let lifts =
            |phase: TouchPhase, pointer: u32| matches!(phase, TouchPhase::Up | TouchPhase::Cancel) && self.pointers.contains_key(&pointer);
        match &message.body {
            Body::Touch { pointer, phase, .. } | Body::Input(InputEvent::Touch { pointer, phase, .. }) => lifts(*phase, *pointer),
            Body::Input(InputEvent::Key { phase: KeyPhase::Up, usage }) => self.keys.contains(usage),
            Body::Input(InputEvent::Button { phase: ButtonPhase::Up, button }) => self.buttons.contains(button),
            _ => false,
        }
    }

    /// Lift everything still down: a cancel for each pointer, an up for each held key and button.
    /// Call it when the channel closes or is replaced and when the session ends (it also runs on
    /// drop), so a viewer that vanishes mid-gesture never leaves the device with input stuck.
    /// Moves waiting for their interval are discarded; the cancel carries the last position.
    pub fn release_all(&mut self) {
        self.pending.clear();
        self.last_move.clear();
        let mut pointers: Vec<(u32, Point)> = self.pointers.drain().collect();
        pointers.sort_by_key(|(id, _)| *id);
        for (pointer, point) in pointers {
            let _ = self.sink.input(&InputEvent::Touch { pointer, phase: TouchPhase::Cancel, point });
        }
        let mut keys: Vec<u16> = self.keys.drain().collect();
        keys.sort_unstable();
        for usage in keys {
            let _ = self.sink.input(&InputEvent::Key { phase: KeyPhase::Up, usage });
        }
        let buttons: Vec<Button> = self.buttons.drain().collect();
        for button in buttons {
            let _ = self.sink.input(&InputEvent::Button { button, phase: ButtonPhase::Up });
        }
    }

    /// Deliver the coalesced moves whose interval has ended.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<Reply>) {
        let due: Vec<u32> = self.pending.iter().filter(|m| self.move_due(m.pointer, now)).map(|m| m.pointer).collect();
        for pointer in due {
            self.flush_pointer(pointer, now, out);
        }
    }

    /// When [`poll`](Self::poll) next has something to deliver.
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.iter().filter_map(|m| self.last_move.get(&m.pointer).map(|l| *l + self.config.min_move_interval)).min()
    }

    /// The pointers that are down.
    pub fn pointers_down(&self) -> usize {
        self.pointers.len()
    }

    fn take_token(&mut self, now: Instant) -> bool {
        let burst = f64::from(self.config.burst.max(1));
        if let Some(last) = self.refilled {
            let refill = now.duration_since(last).as_secs_f64() * f64::from(self.config.max_messages_per_second);
            self.tokens = (self.tokens + refill).min(burst);
        }
        self.refilled = Some(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn move_due(&self, pointer: u32, now: Instant) -> bool {
        self.last_move.get(&pointer).is_none_or(|l| now >= *l + self.config.min_move_interval)
    }

    fn handle(&mut self, now: Instant, rx: f64, message: Message, out: &mut Vec<Reply>) {
        let Message { seq, t, body } = message;
        let refuse = |out: &mut Vec<Reply>, code, message: String| out.push(Reply::Error { seq, code, message });
        let (pointer, phase, point) = match body {
            Body::Ping => {
                out.push(Reply::Pong { seq, t, rx });
                return;
            }
            Body::Touch { pointer, phase, point } => (pointer, phase, point),
            Body::Input(InputEvent::Touch { pointer, phase, point }) => (pointer, phase, Some(point)),
            Body::Input(event) => {
                self.flush_all(now, out);
                let delivered = self.deliver(&event, seq, t, rx, out);
                match event {
                    InputEvent::Key { phase: KeyPhase::Down, usage } if delivered => {
                        self.keys.insert(usage);
                    }
                    InputEvent::Key { phase: KeyPhase::Up, usage } => {
                        self.keys.remove(&usage);
                    }
                    InputEvent::Button { button, phase: ButtonPhase::Down } if delivered => {
                        self.buttons.insert(button);
                    }
                    InputEvent::Button { button, phase: ButtonPhase::Up } => {
                        self.buttons.remove(&button);
                    }
                    _ => {}
                }
                return;
            }
        };
        match phase {
            TouchPhase::Down => {
                if self.pointers.contains_key(&pointer) {
                    return refuse(out, ErrorCode::Pointer, format!("pointer {pointer} is already down"));
                }
                if self.pointers.len() >= self.config.max_pointers {
                    return refuse(out, ErrorCode::Pointer, format!("{} pointers are already down", self.pointers.len()));
                }
                let Some(point) = point else { return refuse(out, ErrorCode::Field, "a down needs a point".into()) };
                self.flush_all(now, out);
                if self.deliver(&InputEvent::Touch { pointer, phase, point }, seq, t, rx, out) {
                    self.pointers.insert(pointer, point);
                    self.last_move.remove(&pointer);
                }
            }
            TouchPhase::Move => {
                let Some(point) = point else { return refuse(out, ErrorCode::Field, "a move needs a point".into()) };
                let Some(last) = self.pointers.get_mut(&pointer) else {
                    return refuse(out, ErrorCode::Pointer, format!("pointer {pointer} is not down"));
                };
                *last = point;
                let queued = PendingMove { pointer, point, seq, t, rx };
                match self.pending.iter_mut().find(|m| m.pointer == pointer) {
                    Some(slot) => *slot = queued,
                    None => self.pending.push(queued),
                }
                if self.move_due(pointer, now) {
                    self.flush_pointer(pointer, now, out);
                }
            }
            TouchPhase::Up | TouchPhase::Cancel => {
                let Some(last) = self.pointers.get(&pointer).copied() else {
                    return refuse(out, ErrorCode::Pointer, format!("pointer {pointer} is not down"));
                };
                self.flush_all(now, out);
                // The viewer's contact is gone whatever the sink says: never leave it stuck down.
                self.pointers.remove(&pointer);
                self.last_move.remove(&pointer);
                self.deliver(&InputEvent::Touch { pointer, phase, point: point.unwrap_or(last) }, seq, t, rx, out);
            }
        }
    }

    fn flush_all(&mut self, now: Instant, out: &mut Vec<Reply>) {
        for m in std::mem::take(&mut self.pending) {
            self.deliver_move(m, now, out);
        }
    }

    fn flush_pointer(&mut self, pointer: u32, now: Instant, out: &mut Vec<Reply>) {
        if let Some(i) = self.pending.iter().position(|m| m.pointer == pointer) {
            let m = self.pending.remove(i);
            self.deliver_move(m, now, out);
        }
    }

    fn deliver_move(&mut self, m: PendingMove, now: Instant, out: &mut Vec<Reply>) {
        self.last_move.insert(m.pointer, now);
        let event = InputEvent::Touch { pointer: m.pointer, phase: TouchPhase::Move, point: m.point };
        self.deliver(&event, m.seq, m.t, m.rx, out);
    }

    fn deliver(&mut self, event: &InputEvent, seq: Option<u64>, t: Option<f64>, rx: f64, out: &mut Vec<Reply>) -> bool {
        match self.sink.input(event) {
            Ok(()) => {
                if t.is_some() {
                    out.push(Reply::Ack { seq, t, rx, done: wall_ms() });
                }
                true
            }
            Err(message) => {
                out.push(Reply::Error { seq, code: ErrorCode::Sink, message });
                false
            }
        }
    }
}

/// Unix time in milliseconds (0 before the epoch, which a clock set that far back deserves).
fn wall_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

// --- A minimal JSON reader and writer: the messages are small flat objects, and the crate has no
// serde (sim-rtc keeps its dependency tree to str0m).

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

impl Value {
    fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Num(n) if n.fract() == 0.0 && (0.0..=MAX_SAFE_INTEGER).contains(n) => Some(*n as u64),
            _ => None,
        }
    }
}

/// A parsed message object, with typed field access that turns into protocol errors.
struct Obj(Vec<(String, Value)>);

impl Obj {
    fn parse(text: &str) -> Result<Obj, ProtocolError> {
        let mut p = Parser { s: text.as_bytes(), i: 0 };
        let v = p.value(0).map_err(|m| ProtocolError::new(ErrorCode::Json, m))?;
        p.ws();
        if p.i != p.s.len() {
            return Err(ProtocolError::new(ErrorCode::Json, format!("trailing data at byte {}", p.i)));
        }
        match v {
            Value::Obj(fields) => Ok(Obj(fields)),
            _ => Err(ProtocolError::new(ErrorCode::Json, "a message is a JSON object")),
        }
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn version(&self) -> Result<(), ProtocolError> {
        match self.get("v") {
            None => Err(ProtocolError::new(ErrorCode::Version, format!("no `v`: this server speaks version {VERSION}"))),
            Some(v) if v.as_u64() == Some(u64::from(VERSION)) => Ok(()),
            Some(v) => {
                Err(ProtocolError::new(ErrorCode::Version, format!("version {} is not supported: this server speaks {VERSION}", show(v))))
            }
        }
    }

    fn opt_f64(&self, key: &str) -> Result<Option<f64>, ProtocolError> {
        match self.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Num(n)) if n.is_finite() => Ok(Some(*n)),
            Some(v) => Err(field(format!("`{key}` must be a number, not {}", show(v)))),
        }
    }

    fn req_f64(&self, key: &str) -> Result<f64, ProtocolError> {
        self.opt_f64(key)?.ok_or_else(|| field(format!("`{key}` is missing")))
    }

    fn opt_u64(&self, key: &str) -> Result<Option<u64>, ProtocolError> {
        match self.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v.as_u64().map(Some).ok_or_else(|| field(format!("`{key}` must be a non-negative integer, not {}", show(v)))),
        }
    }

    fn req_u64(&self, key: &str) -> Result<u64, ProtocolError> {
        self.opt_u64(key)?.ok_or_else(|| field(format!("`{key}` is missing")))
    }

    fn opt_str(&self, key: &str) -> Result<Option<&str>, ProtocolError> {
        match self.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s)),
            Some(v) => Err(field(format!("`{key}` must be a string, not {}", show(v)))),
        }
    }

    fn req_str(&self, key: &str) -> Result<&str, ProtocolError> {
        self.opt_str(key)?.ok_or_else(|| field(format!("`{key}` is missing")))
    }

    fn point(&self) -> Result<Point, ProtocolError> {
        let unit = |k: &str| -> Result<f64, ProtocolError> {
            let n = self.req_f64(k)?;
            if !(0.0..=1.0).contains(&n) {
                return Err(field(format!("`{k}` {n} is outside 0..=1 (coordinates are normalised to the frame)")));
            }
            Ok(n)
        };
        let side = |k: &str| -> Result<u32, ProtocolError> {
            let n = self.req_u64(k)?;
            u32::try_from(n)
                .ok()
                .filter(|n| (1..=MAX_FRAME_SIDE).contains(n))
                .ok_or_else(|| field(format!("`{k}` {n} is not a frame side")))
        };
        Ok(Point { x: unit("x")?, y: unit("y")?, frame: FrameSize::new(side("w")?, side("h")?) })
    }
}

/// A value as an error message shows it (strings shortened).
fn show(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Num(n) => n.to_string(),
        Value::Str(s) => format!("{:?}", s.chars().take(32).collect::<String>()),
        Value::Arr(_) => "an array".into(),
        Value::Obj(_) => "an object".into(),
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.s.get(self.i) {
            self.i += 1;
        }
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.s.get(self.i) == Some(&b) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn literal(&mut self, word: &str, v: Value) -> Result<Value, String> {
        if self.s[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(format!("unexpected byte at {}", self.i))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err("nested too deep".into());
        }
        self.ws();
        match self.s.get(self.i) {
            None => Err("unexpected end".into()),
            Some(b'{') => {
                self.i += 1;
                let mut fields: Vec<(String, Value)> = Vec::new();
                self.ws();
                if self.eat(b'}') {
                    return Ok(Value::Obj(fields));
                }
                loop {
                    self.ws();
                    let key = self.string()?;
                    if fields.iter().any(|(k, _)| *k == key) {
                        return Err(format!("duplicate key {key:?}"));
                    }
                    self.ws();
                    if !self.eat(b':') {
                        return Err(format!("expected ':' at byte {}", self.i));
                    }
                    let v = self.value(depth + 1)?;
                    fields.push((key, v));
                    self.ws();
                    if self.eat(b'}') {
                        return Ok(Value::Obj(fields));
                    }
                    if !self.eat(b',') {
                        return Err(format!("expected ',' or '}}' at byte {}", self.i));
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.eat(b']') {
                    return Ok(Value::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    if self.eat(b']') {
                        return Ok(Value::Arr(items));
                    }
                    if !self.eat(b',') {
                        return Err(format!("expected ',' or ']' at byte {}", self.i));
                    }
                }
            }
            Some(b'"') => self.string().map(Value::Str),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(format!("unexpected byte at {}", self.i)),
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        let digits = |p: &mut Parser<'_>| {
            let from = p.i;
            while let Some(b'0'..=b'9') = p.s.get(p.i) {
                p.i += 1;
            }
            p.i > from
        };
        self.eat(b'-');
        if self.s.get(self.i) == Some(&b'0') && self.s.get(self.i + 1).is_some_and(u8::is_ascii_digit) {
            return Err(format!("leading zero in a number at byte {start}"));
        }
        if !digits(self) {
            return Err(format!("bad number at byte {start}"));
        }
        if self.eat(b'.') && !digits(self) {
            return Err(format!("bad number at byte {start}"));
        }
        if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            let _ = self.eat(b'+') || self.eat(b'-');
            if !digits(self) {
                return Err(format!("bad number at byte {start}"));
            }
        }
        // Only ASCII digits and signs were consumed, so this is valid UTF-8.
        let text = std::str::from_utf8(&self.s[start..self.i]).map_err(|_| format!("bad number at byte {start}"))?;
        text.parse::<f64>().map(Value::Num).map_err(|_| format!("bad number at byte {start}"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self.s.get(self.i..self.i + 4).ok_or("short \\u escape")?;
        if !digits.iter().all(u8::is_ascii_hexdigit) {
            return Err("bad \\u escape".into());
        }
        let text = std::str::from_utf8(digits).map_err(|_| "bad \\u escape")?;
        let n = u32::from_str_radix(text, 16).map_err(|_| "bad \\u escape")?;
        self.i += 4;
        Ok(n)
    }

    fn string(&mut self) -> Result<String, String> {
        if !self.eat(b'"') {
            return Err(format!("expected a string at byte {}", self.i));
        }
        let mut out = String::new();
        loop {
            // Copy the run up to the next quote or backslash in one go.
            let run = self.s[self.i..].iter().position(|b| matches!(b, b'"' | b'\\' | 0..=0x1f)).ok_or("unterminated string")?;
            out.push_str(std::str::from_utf8(&self.s[self.i..self.i + run]).map_err(|_| "not UTF-8")?);
            self.i += run;
            match self.s[self.i] {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let esc = *self.s.get(self.i).ok_or("unterminated string")?;
                    self.i += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let c = if (0xd800..0xdc00).contains(&hi) {
                                if !(self.eat(b'\\') && self.eat(b'u')) {
                                    return Err("unpaired surrogate".into());
                                }
                                let lo = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return Err("unpaired surrogate".into());
                                }
                                0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                            } else {
                                hi
                            };
                            out.push(char::from_u32(c).ok_or("unpaired surrogate")?);
                        }
                        _ => return Err(format!("bad escape at byte {}", self.i - 1)),
                    }
                }
                _ => return Err("control character in a string".into()),
            }
        }
    }
}

/// Builds one flat JSON object.
struct JsonWriter(String);

impl JsonWriter {
    fn new() -> JsonWriter {
        JsonWriter(String::from("{"))
    }

    fn key(&mut self, key: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        escape(&mut self.0, key);
        self.0.push(':');
    }

    fn raw(&mut self, key: &str, json: &str) {
        self.key(key);
        self.0.push_str(json);
    }

    fn num(&mut self, key: &str, n: f64) {
        self.key(key);
        if n.is_finite() {
            let _ = write!(self.0, "{n}");
        } else {
            self.0.push_str("null");
        }
    }

    fn opt_num(&mut self, key: &str, n: Option<f64>) {
        if let Some(n) = n {
            self.num(key, n);
        }
    }

    fn str(&mut self, key: &str, s: &str) {
        self.key(key);
        escape(&mut self.0, s);
    }

    fn finish(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

fn escape(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const FRAME: FrameSize = FrameSize::new(590, 1280);

    fn point(x: f64, y: f64) -> Point {
        Point { x, y, frame: FRAME }
    }

    fn msg(seq: u64, body: Body) -> Message {
        Message { seq: Some(seq), t: Some(seq as f64 * 10.0), body }
    }

    #[test]
    fn every_kind_round_trips_through_json() {
        let bodies = [
            Body::Ping,
            Body::Touch { pointer: 3, phase: TouchPhase::Down, point: Some(point(0.25, 0.75)) },
            Body::Touch { pointer: 3, phase: TouchPhase::Move, point: Some(point(0.5, 0.5)) },
            Body::Touch { pointer: 3, phase: TouchPhase::Up, point: None },
            Body::Touch { pointer: 4, phase: TouchPhase::Cancel, point: Some(point(1.0, 0.0)) },
            Body::Input(InputEvent::Key { phase: KeyPhase::Down, usage: 0x28 }),
            Body::Input(InputEvent::Key { phase: KeyPhase::Up, usage: 0xe1 }),
            Body::Input(InputEvent::Text("héllo \"wörld\"\n\u{1f600}".into())),
            Body::Input(InputEvent::Button { button: Button::VolumeDown, phase: ButtonPhase::Press }),
            Body::Input(InputEvent::Button { button: Button::Back, phase: ButtonPhase::Down }),
            Body::Input(InputEvent::Rotate(Orientation::LandscapeLeft)),
            Body::Input(InputEvent::Fold(Posture::HalfOpen)),
            Body::Input(InputEvent::Wheel { point: point(0.5, 0.5), dx: 0.0, dy: -120.5 }),
        ];
        for (i, body) in bodies.into_iter().enumerate() {
            let m = msg(i as u64, body);
            let json = m.to_json();
            assert_eq!(decode(&json).expect(&json), m, "{json}");
        }
        assert_eq!(
            msg(7, Body::Touch { pointer: 0, phase: TouchPhase::Down, point: Some(point(0.5, 0.25)) }).to_json(),
            r#"{"v":1,"seq":7,"t":70,"kind":"touch","phase":"down","id":0,"x":0.5,"y":0.25,"w":590,"h":1280}"#
        );
    }

    #[test]
    fn decodes_what_a_browser_writes() {
        let m = decode(r#" { "kind" : "key", "v": 1, "phase": "down", "key": "KeyA", "extra": [1, {"a": null}] } "#).expect("key");
        assert_eq!(m.body, Body::Input(InputEvent::Key { phase: KeyPhase::Down, usage: 0x04 }));
        assert_eq!((m.seq, m.t), (None, None));
        let m = decode(r#"{"v":1,"kind":"button","button":"home"}"#).expect("button");
        assert_eq!(m.body, Body::Input(InputEvent::Button { button: Button::Home, phase: ButtonPhase::Press }), "press by default");
        let m = decode(r#"{"v":1,"kind":"text","text":"\ud83d\ude00\u00e9"}"#).expect("surrogates");
        assert_eq!(m.body, Body::Input(InputEvent::Text("\u{1f600}é".into())));
        let m = decode(r#"{"v":1,"kind":"wheel","x":0,"y":1,"w":96,"h":64,"dy":3e1}"#).expect("wheel");
        assert_eq!(
            m.body,
            Body::Input(InputEvent::Wheel { point: Point { x: 0.0, y: 1.0, frame: FrameSize::new(96, 64) }, dx: 0.0, dy: 30.0 })
        );
    }

    #[test]
    fn named_keys_are_hid_usages() {
        let cases = [
            ("KeyA", 0x04),
            ("KeyZ", 0x1d),
            ("Digit1", 0x1e),
            ("Digit9", 0x26),
            ("Digit0", 0x27),
            ("Enter", 0x28),
            ("Escape", 0x29),
            ("Backspace", 0x2a),
            ("Tab", 0x2b),
            (" ", 0x2c),
            ("F1", 0x3a),
            ("F12", 0x45),
            ("ArrowUp", 0x52),
            ("Shift", 0xe1),
            ("MetaRight", 0xe7),
        ];
        for (name, usage) in cases {
            assert_eq!(named_key_usage(name), Some(usage), "{name}");
        }
        for name in ["Keya", "KeyAB", "F13", "F0", "Digit", "a", "Unidentified", ""] {
            assert_eq!(named_key_usage(name), None, "{name}");
        }
    }

    #[test]
    fn refuses_bad_messages_with_a_code_and_the_seq() {
        let cases: &[(&str, ErrorCode)] = &[
            ("", ErrorCode::Json),
            ("[1]", ErrorCode::Json),
            ("{\"v\":1,", ErrorCode::Json),
            ("{\"v\":1} x", ErrorCode::Json),
            ("{\"v\":1,\"v\":1}", ErrorCode::Json),
            ("{\"a\":[[[[[[1]]]]]]}", ErrorCode::Json),
            ("{\"v\":1,\"kind\":\"text\",\"text\":\"\\ud800\"}", ErrorCode::Json),
            (r#"{"kind":"ping","seq":5}"#, ErrorCode::Version),
            (r#"{"v":2,"kind":"ping","seq":5}"#, ErrorCode::Version),
            (r#"{"v":"1","kind":"ping","seq":5}"#, ErrorCode::Version),
            (r#"{"v":1,"kind":"teleport","seq":5}"#, ErrorCode::Kind),
            (r#"{"v":1,"seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"touch","phase":"down","id":0,"x":1.5,"y":0,"w":10,"h":10,"seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"touch","phase":"down","id":0,"x":0.5,"y":0,"w":0,"h":10,"seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"touch","phase":"down","id":-1,"x":0.5,"y":0,"w":10,"h":10,"seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"touch","phase":"move","id":0,"seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"touch","phase":"hover","id":0,"seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"key","phase":"down","usage":2,"seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"key","phase":"down","key":"Hyper","seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"key","phase":"down","key":"KeyA","usage":4,"seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"text","text":"","seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"button","button":"eject","seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"rotate","orientation":"sideways","seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"fold","posture":"origami","seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"wheel","x":0.5,"y":0.5,"w":10,"h":10,"dy":1e9,"seq":5}"#, ErrorCode::Field),
            (r#"{"v":1,"kind":"ping","t":"now","seq":5}"#, ErrorCode::Field),
        ];
        for (text, code) in cases {
            let e = decode(text).expect_err(text);
            assert_eq!(e.code, *code, "{text}: {e}");
            if text.contains("\"seq\":5") {
                assert_eq!(e.seq, Some(5), "{text}");
            }
        }
    }

    #[test]
    fn replies_round_trip() {
        let replies = [
            Reply::Hello { versions: vec![1] },
            Reply::Ack { seq: Some(3), t: Some(1.5), rx: 1_700_000_000_123.25, done: 1_700_000_000_124.0 },
            Reply::Pong { seq: None, t: Some(2.0), rx: 5.0 },
            Reply::Error { seq: Some(9), code: ErrorCode::Kind, message: "unknown kind \"x\"".into() },
        ];
        for r in replies {
            assert_eq!(Reply::decode(&r.to_json()).expect("reply"), r);
        }
        assert_eq!(Reply::Hello { versions: vec![1] }.to_json(), r#"{"v":1,"kind":"hello","protocol":"simee-input","versions":[1]}"#);
    }

    #[test]
    fn maps_stream_points_to_device_pixels_through_the_downscale() {
        let iphone = DisplayMap::new(1179, 2556);
        let seen = iphone.stream_size(Some(1280));
        assert_eq!(seen, FrameSize::new(590, 1280), "the encoder's rule");
        assert_eq!(DisplayMap::new(2560, 1600).stream_size(Some(1280)), FrameSize::new(1280, 800));
        assert_eq!(DisplayMap::new(1080, 1920).stream_size(None), FrameSize::new(1080, 1920));
        let at = |x, y| iphone.to_device(&Point { x, y, frame: seen }).expect("same shape");
        assert_eq!(at(0.5, 0.5), (589.5, 1278.0));
        assert_eq!(at(0.0, 0.0), (0.0, 0.0));
        assert_eq!(at(1.0, 1.0), (1178.0, 2555.0), "clamped inside the display");
        // A viewer showing the video at another size sends the same normalised point.
        assert_eq!(iphone.to_device(&Point { x: 0.5, y: 0.5, frame: FrameSize::new(295, 640) }).expect("half size"), (589.5, 1278.0));
        // The device rotated: a point from the portrait frame is refused, not stretched.
        let landscape = DisplayMap::new(2556, 1179);
        let err = landscape.to_device(&Point { x: 0.5, y: 0.5, frame: seen }).expect_err("stale");
        assert_eq!(err, StaleFrame { seen, device: landscape.device });
        assert!(err.to_string().contains("590x1280"), "{err}");
        // Wheel deltas scale by the downscale.
        assert_eq!(iphone.delta_to_device(10.0, -64.0, seen), (10.0 * 1179.0 / 590.0, -64.0 * 2556.0 / 1280.0));
        // Tiny frames with even-side rounding still match.
        assert!(DisplayMap::new(97, 65).matches(FrameSize::new(96, 64)));
    }

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<InputEvent>>>);

    impl InputSink for Recorder {
        fn input(&mut self, event: &InputEvent) -> Result<(), String> {
            if let InputEvent::Button { button: Button::Back, .. } = event {
                return Err("this device has no back button".into());
            }
            self.0.lock().map_err(|e| e.to_string())?.push(event.clone());
            Ok(())
        }
    }

    impl Recorder {
        fn take(&self) -> Vec<InputEvent> {
            std::mem::take(&mut *self.0.lock().expect("lock"))
        }
    }

    fn session(config: InputConfig) -> (InputSession, Recorder) {
        let rec = Recorder::default();
        (InputSession::new(rec.clone(), config), rec)
    }

    fn touch(pointer: u32, phase: TouchPhase, x: f64) -> InputEvent {
        InputEvent::Touch { pointer, phase, point: point(x, 0.5) }
    }

    fn send(s: &mut InputSession, now: Instant, m: &Message) -> Vec<Reply> {
        let mut out = Vec::new();
        s.receive(now, false, m.to_json().as_bytes(), &mut out);
        out
    }

    fn touch_msg(seq: u64, pointer: u32, phase: TouchPhase, x: f64) -> Message {
        Message { seq: Some(seq), t: None, body: Body::Touch { pointer, phase, point: Some(point(x, 0.5)) } }
    }

    #[test]
    fn coalesces_moves_per_pointer_and_keeps_order() {
        let ms = Duration::from_millis;
        let (mut s, rec) = session(InputConfig { min_move_interval: ms(10), ..InputConfig::default() });
        let t0 = Instant::now();
        assert!(send(&mut s, t0, &touch_msg(1, 0, TouchPhase::Down, 0.1)).is_empty());
        assert!(send(&mut s, t0, &touch_msg(2, 0, TouchPhase::Move, 0.2)).is_empty(), "the first move goes straight through");
        send(&mut s, t0 + ms(1), &touch_msg(3, 0, TouchPhase::Move, 0.3));
        send(&mut s, t0 + ms(2), &touch_msg(4, 0, TouchPhase::Move, 0.4));
        // A second finger moves on its own clock.
        send(&mut s, t0 + ms(2), &touch_msg(5, 1, TouchPhase::Down, 0.9));
        assert_eq!(
            rec.take(),
            [
                touch(0, TouchPhase::Down, 0.1),
                touch(0, TouchPhase::Move, 0.2),
                touch(0, TouchPhase::Move, 0.4),
                touch(1, TouchPhase::Down, 0.9)
            ],
            "a down flushes the pending move first"
        );
        send(&mut s, t0 + ms(3), &touch_msg(6, 0, TouchPhase::Move, 0.5));
        send(&mut s, t0 + ms(4), &touch_msg(7, 0, TouchPhase::Move, 0.6));
        send(&mut s, t0 + ms(4), &touch_msg(8, 1, TouchPhase::Move, 0.8));
        assert_eq!(rec.take(), [touch(1, TouchPhase::Move, 0.8)]);
        assert_eq!(s.next_due(), Some(t0 + ms(12)), "pointer 0's last delivered move was at 2 ms");
        let mut out = Vec::new();
        s.poll(t0 + ms(11), &mut out);
        assert!(rec.take().is_empty());
        s.poll(t0 + ms(12), &mut out);
        assert_eq!(rec.take(), [touch(0, TouchPhase::Move, 0.6)], "only the latest of the coalesced moves");
        assert_eq!(s.next_due(), None);
        // A lift without a point lifts where the pointer was.
        send(&mut s, t0 + ms(13), &Message { seq: None, t: None, body: Body::Touch { pointer: 0, phase: TouchPhase::Up, point: None } });
        assert_eq!(rec.take(), [touch(0, TouchPhase::Up, 0.6)]);
        assert_eq!(s.pointers_down(), 1);
        assert!(out.is_empty());
    }

    #[test]
    fn acks_echo_the_viewer_time_and_refusals_carry_the_seq() {
        let (mut s, rec) = session(InputConfig::default());
        let now = Instant::now();
        let before = wall_ms();
        let ping = send(&mut s, now, &Message { seq: Some(1), t: Some(123.5), body: Body::Ping });
        match ping.as_slice() {
            [Reply::Pong { seq: Some(1), t: Some(t), rx }] => assert!(*t == 123.5 && *rx >= before, "{ping:?}"),
            other => panic!("{other:?}"),
        }
        let key = Message { seq: Some(2), t: Some(200.0), body: Body::Input(InputEvent::Key { phase: KeyPhase::Down, usage: 4 }) };
        match send(&mut s, now, &key).as_slice() {
            [Reply::Ack { seq: Some(2), t: Some(t), rx, done }] => assert!(*t == 200.0 && rx <= done && *rx >= before),
            other => panic!("{other:?}"),
        }
        let back = Message {
            seq: Some(3),
            t: Some(1.0),
            body: Body::Input(InputEvent::Button { button: Button::Back, phase: ButtonPhase::Press }),
        };
        assert!(matches!(send(&mut s, now, &back).as_slice(), [Reply::Error { seq: Some(3), code: ErrorCode::Sink, .. }]));
        let stray = touch_msg(4, 7, TouchPhase::Move, 0.5);
        assert!(matches!(send(&mut s, now, &stray).as_slice(), [Reply::Error { seq: Some(4), code: ErrorCode::Pointer, .. }]));
        send(&mut s, now, &touch_msg(5, 7, TouchPhase::Down, 0.5));
        assert!(matches!(
            send(&mut s, now, &touch_msg(6, 7, TouchPhase::Down, 0.5)).as_slice(),
            [Reply::Error { seq: Some(6), code: ErrorCode::Pointer, .. }]
        ));
        let mut out = Vec::new();
        s.receive(now, true, b"{}", &mut out);
        s.receive(now, false, &[0xff, 0xfe], &mut out);
        s.receive(now, false, br#"{"v":9,"kind":"ping","seq":8}"#, &mut out);
        s.receive(now, false, br#"{"v":1,"kind":"warp","seq":9}"#, &mut out);
        s.receive(now, false, &[b' '; 5000], &mut out);
        let codes: Vec<_> = out
            .iter()
            .map(|r| match r {
                Reply::Error { code, seq, .. } => (*code, *seq),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            codes,
            [
                (ErrorCode::Encoding, None),
                (ErrorCode::Encoding, None),
                (ErrorCode::Version, Some(8)),
                (ErrorCode::Kind, Some(9)),
                (ErrorCode::Size, None)
            ]
        );
        assert_eq!(rec.take(), [InputEvent::Key { phase: KeyPhase::Down, usage: 4 }, touch(7, TouchPhase::Down, 0.5)]);
    }

    #[test]
    fn rate_limits_with_one_error_per_second() {
        let (mut s, rec) = session(InputConfig { max_messages_per_second: 10, burst: 3, ..InputConfig::default() });
        let t0 = Instant::now();
        let key = Message { seq: None, t: None, body: Body::Input(InputEvent::Key { phase: KeyPhase::Up, usage: 4 }) };
        let mut out = Vec::new();
        for _ in 0..6 {
            out.extend(send(&mut s, t0, &key));
        }
        assert_eq!(rec.take().len(), 3, "the burst");
        assert!(matches!(out.as_slice(), [Reply::Error { code: ErrorCode::Rate, .. }]), "{out:?}");
        assert_eq!(send(&mut s, t0 + Duration::from_millis(100), &key), [], "one token back after 100 ms");
        assert!(send(&mut s, t0 + Duration::from_millis(100), &key).is_empty(), "dropped, error already sent this second");
        assert_eq!(rec.take().len(), 1);
        assert!(matches!(send(&mut s, t0 + Duration::from_millis(1100), &key).as_slice(), []));
        assert_eq!(rec.take().len(), 1);
    }

    #[test]
    fn a_lift_the_sink_refuses_still_frees_the_pointer() {
        let now = Instant::now();
        let mut s2 = InputSession::new(
            |e: &InputEvent| match e {
                InputEvent::Touch { phase: TouchPhase::Up, .. } => Err("device gone".to_owned()),
                _ => Ok(()),
            },
            InputConfig::default(),
        );
        send(&mut s2, now, &touch_msg(1, 0, TouchPhase::Down, 0.5));
        assert!(matches!(
            send(&mut s2, now, &touch_msg(2, 0, TouchPhase::Up, 0.5)).as_slice(),
            [Reply::Error { code: ErrorCode::Sink, .. }]
        ));
        assert_eq!(s2.pointers_down(), 0);
        assert!(send(&mut s2, now, &touch_msg(3, 0, TouchPhase::Down, 0.5)).is_empty(), "the pointer can go down again");
    }

    fn key_msg(phase: KeyPhase, usage: u16) -> Message {
        Message { seq: None, t: None, body: Body::Input(InputEvent::Key { phase, usage }) }
    }

    #[test]
    fn release_all_cancels_pointers_and_lifts_keys_and_buttons() {
        let (mut s, rec) = session(InputConfig::default());
        let now = Instant::now();
        send(&mut s, now, &touch_msg(1, 0, TouchPhase::Down, 0.25));
        send(&mut s, now, &key_msg(KeyPhase::Down, 0xe1));
        let power =
            Message { seq: None, t: None, body: Body::Input(InputEvent::Button { button: Button::Power, phase: ButtonPhase::Down }) };
        send(&mut s, now, &power);
        rec.take();
        s.release_all();
        assert_eq!(
            rec.take(),
            vec![
                touch(0, TouchPhase::Cancel, 0.25),
                InputEvent::Key { phase: KeyPhase::Up, usage: 0xe1 },
                InputEvent::Button { button: Button::Power, phase: ButtonPhase::Up },
            ]
        );
        s.release_all();
        assert!(rec.take().is_empty(), "nothing left to lift");
        // A reopened channel's first down is accepted.
        assert!(send(&mut s, now, &touch_msg(2, 0, TouchPhase::Down, 0.5)).is_empty());
        assert_eq!(rec.take(), vec![touch(0, TouchPhase::Down, 0.5)]);
    }

    #[test]
    fn dropping_a_session_lifts_what_is_down() {
        let (mut s, rec) = session(InputConfig::default());
        send(&mut s, Instant::now(), &touch_msg(1, 3, TouchPhase::Down, 0.5));
        rec.take();
        drop(s);
        assert_eq!(rec.take(), vec![touch(3, TouchPhase::Cancel, 0.5)]);
    }

    #[test]
    fn the_rate_limit_never_drops_a_lift() {
        let (mut s, rec) = session(InputConfig { max_messages_per_second: 10, burst: 3, ..InputConfig::default() });
        let t0 = Instant::now();
        send(&mut s, t0, &touch_msg(1, 0, TouchPhase::Down, 0.5));
        send(&mut s, t0, &key_msg(KeyPhase::Down, 4));
        send(&mut s, t0, &touch_msg(2, 0, TouchPhase::Move, 0.6));
        rec.take();
        // Out of tokens: a down is dropped with the seq in the error, the lifts get through.
        let out = send(&mut s, t0, &touch_msg(9, 1, TouchPhase::Down, 0.5));
        assert!(matches!(out.as_slice(), [Reply::Error { seq: Some(9), code: ErrorCode::Rate, .. }]), "{out:?}");
        assert!(send(&mut s, t0, &touch_msg(10, 0, TouchPhase::Up, 0.6)).is_empty());
        assert!(send(&mut s, t0, &key_msg(KeyPhase::Up, 4)).is_empty());
        let events = rec.take();
        assert!(events.contains(&touch(0, TouchPhase::Up, 0.6)), "{events:?}");
        assert!(events.contains(&InputEvent::Key { phase: KeyPhase::Up, usage: 4 }), "{events:?}");
        assert_eq!(s.pointers_down(), 0);
    }

    #[test]
    fn the_reader_refuses_plus_signed_escapes_and_leading_zeros() {
        assert!(decode(r#"{"v":1,"kind":"text","text":"\u+041"}"#).is_err());
        assert!(decode(r#"{"v":01,"kind":"ping"}"#).is_err());
        assert!(decode(r#"{"v":1,"kind":"ping","seq":007}"#).is_err());
        assert!(decode(r#"{"v":1,"kind":"ping","seq":0}"#).is_ok());
    }

    #[test]
    fn matches_within_the_encoders_rounding_at_extreme_aspects() {
        for (w, h, max) in [(4000, 20, 100), (20, 4000, 100), (1179, 2556, 1280), (3, 5000, 1000), (1000, 1000, 64)] {
            let map = DisplayMap::new(w, h);
            let seen = map.stream_size(Some(max));
            assert!(map.matches(seen), "{w}x{h} streamed as {seen} should match");
        }
        let wide = DisplayMap::new(4000, 20);
        assert!(!wide.matches(FrameSize::new(16, 100)), "a rotated extreme aspect is stale");
        assert!(!DisplayMap::new(1179, 2556).matches(FrameSize::new(1280, 590)));
    }
}
