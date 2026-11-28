//! The live display stream between a device process and a front end: frames one way, input lines
//! the other. The Android emulator serves it on `<avd>/simee/display.sock`; the desktop window
//! reads it.
//!
//! Client → server, one text line each ([`Input`]):
//! - `hello <max_width>`: start streaming, downscaled by a whole factor to at most that width;
//! - `shm`: send frames through a shared-memory ring instead of the socket (see below);
//! - `socket`: send frames on the socket again (a client that couldn't open the ring);
//! - `rotates`: send frames as the panel lies and say how to turn them (`EMUO`), for a client
//!   that turns them as it draws; without it a turned device's frames come turned upright;
//! - `touch <x> <y> down|move|up` in pixels of the screen as the guest rotated its display (the
//!   frames' pixels, turned upright when the stream says to turn them);
//! - `key <name>`: press and release a key by its console name (`HOME`, `BACK`, `POWER`, `A`, ...);
//! - `wheel <ticks>` on the rotary / scroll device;
//! - `surfaces <name>`: send frames as IOSurfaces to the Mach port checked in under bootstrap name
//!   `<name>` (`EMUM`, below), where the server has them (`emu -gpu metal`); macOS only.
//!   `surfaces -` ([`NO_SURFACES`]) turns them off again (a client that can't use one it got);
//! - `release <seq>`: the client is done with surface `seq` (`EMUM`): the server may reuse it.
//!
//! Server → client, 32-byte [`Record`]s, little-endian:
//! - `EMUF` ([`FrameHeader`]: u32 width, height, device width, device height, u64 frame number,
//!   u32 byte count), then the frame's RGBA rows on the socket;
//! - `EMUR`: a shared frame ring was created; its path (the u32 byte count of UTF-8) follows;
//! - `EMUS`: a frame whose RGBA rows are in a slot of the last announced ring (the header's last
//!   u32 is the slot), so only these 32 bytes cross the socket;
//! - `EMUO`: the frames after it are to be turned clockwise by the last u32's quarter turns
//!   ([`Rotation`]) to show upright, as the guest rotated its display. Only to clients that sent
//!   `rotates`, before their first frame and whenever it changes;
//! - `EMUM`: a frame the server's GPU copied into an IOSurface (BGRA, full size), whose Mach send
//!   right went to the client's port just before this record, in a message with id
//!   [`SURFACE_MSG_ID`] carrying the surface port and the record's last u32, its sequence number.
//!   Only to clients that sent `surfaces`; each one is held for the client until it sends
//!   `release <seq>`. A frame the server can't hand over that way goes out as `EMUS` or `EMUF`.
//!
//! `EMUR` and `EMUS` go only to clients that sent `shm`; a server that predates it ignores the
//! line and keeps sending `EMUF`, so clients read whichever arrives. The ring ([`ring`]) is a
//! file the server maps and writes; clients read it with `pread`, so they need no mapping.

use crate::gesture::Phase;
use crate::rotation::Rotation;

/// The bytes every frame header starts with.
pub const MAGIC: &[u8; 4] = b"EMUF";
/// A frame in a slot of the shared ring.
pub const SHARED_MAGIC: &[u8; 4] = b"EMUS";
/// A shared ring's announcement.
pub const RING_MAGIC: &[u8; 4] = b"EMUR";
/// How the frames after it are turned.
pub const ROTATION_MAGIC: &[u8; 4] = b"EMUO";
/// A frame handed over as an IOSurface (Mach port).
pub const SURFACE_MAGIC: &[u8; 4] = b"EMUM";
/// `msgh_id` of the Mach message that carries an `EMUM` frame's IOSurface port: "EMUM" as a
/// little-endian u32. Its body is one port descriptor (the surface's send right, moved) and then
/// the record's sequence number as a u32.
pub const SURFACE_MSG_ID: i32 = i32::from_le_bytes(*SURFACE_MAGIC);
/// The `surfaces` name that turns surfaces off for the connection (`surfaces -`): frames go the
/// client's other way from the next one. A client sends it when it can't use a surface it got.
pub const NO_SURFACES: &str = "-";

/// Whether a failed Mach send (`kern_return_t` `kr`) had copied the message in before it failed,
/// so the kernel handed its rights back to the sender (a pseudo-receive, `mach_msg(2)`): a moved
/// port is the sender's again under its name, and a copied destination came back with one more
/// user reference, which the sender has to drop. A send refused before copyin (a dead
/// destination) hands nothing back.
pub fn mach_send_pseudo_received(kr: i32) -> bool {
    // `MACH_MSG_MASK`: the copyout's own error bits, or'ed into the send's code.
    const COPYOUT_BITS: i32 = 0x3e00;
    // MACH_SEND_TIMED_OUT, MACH_SEND_INTERRUPTED, MACH_SEND_INVALID_NOTIFY.
    matches!(kr & !COPYOUT_BITS, 0x1000_0004 | 0x1000_0007 | 0x1000_000b)
}
/// Size of a frame header in bytes.
pub const HEADER_LEN: usize = 32;

/// Describes the RGBA rows that follow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// Size of the (possibly downscaled) frame that follows.
    pub width: u32,
    pub height: u32,
    /// The device's own display size: input coordinates are in these pixels.
    pub device_width: u32,
    pub device_height: u32,
    pub frame: u64,
    /// Byte count of the RGBA rows: `width * height * 4`.
    pub len: u32,
}

impl FrameHeader {
    /// A header for a `width` x `height` RGBA frame; `None` when its byte count overflows a u32.
    pub fn new(width: u32, height: u32, device_width: u32, device_height: u32, frame: u64) -> Option<Self> {
        let len = width.checked_mul(height)?.checked_mul(4)?;
        Some(FrameHeader { width, height, device_width, device_height, frame, len })
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        self.encode_as(MAGIC, self.len)
    }

    fn encode_as(&self, magic: &[u8; 4], last: u32) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(magic);
        for (i, v) in [self.width, self.height, self.device_width, self.device_height].into_iter().enumerate() {
            out[4 + i * 4..8 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        out[20..28].copy_from_slice(&self.frame.to_le_bytes());
        out[28..32].copy_from_slice(&last.to_le_bytes());
        out
    }

    /// Parses and checks a header: the magic, and that the byte count matches the size.
    pub fn decode(b: &[u8; HEADER_LEN]) -> Result<Self, String> {
        match Record::decode(b)? {
            Record::Frame(h) => Ok(h),
            _ => Err("bad frame header".into()),
        }
    }

    fn fields(b: &[u8; HEADER_LEN]) -> (Self, u32) {
        let u = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let mut frame = [0u8; 8];
        frame.copy_from_slice(&b[20..28]);
        let h =
            FrameHeader { width: u(4), height: u(8), device_width: u(12), device_height: u(16), frame: u64::from_le_bytes(frame), len: 0 };
        (h, u(28))
    }
}

/// One server → client record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    /// `EMUF`: the frame's RGBA rows (`len` bytes) follow on the socket.
    Frame(FrameHeader),
    /// `EMUS`: the frame's RGBA rows are in slot `slot` of the announced ring.
    Shared { header: FrameHeader, slot: u32 },
    /// `EMUR`: a ring was created; its path, `path_len` bytes of UTF-8, follows.
    Ring { path_len: u32 },
    /// `EMUO`: the frames after it show upright turned clockwise by this much.
    Rotation(Rotation),
    /// `EMUM`: the frame is IOSurface `seq`, sent to the client's Mach port (see the module docs);
    /// the header's `len` is the BGRA size it would have as tight rows.
    Surface { header: FrameHeader, seq: u32 },
}

impl Record {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        match self {
            Record::Frame(h) => h.encode(),
            Record::Shared { header, slot } => header.encode_as(SHARED_MAGIC, *slot),
            Record::Surface { header, seq } => header.encode_as(SURFACE_MAGIC, *seq),
            Record::Ring { path_len } => {
                let mut out = [0u8; HEADER_LEN];
                out[0..4].copy_from_slice(RING_MAGIC);
                out[28..32].copy_from_slice(&path_len.to_le_bytes());
                out
            }
            Record::Rotation(r) => {
                let mut out = [0u8; HEADER_LEN];
                out[0..4].copy_from_slice(ROTATION_MAGIC);
                out[28..32].copy_from_slice(&r.quarters().to_le_bytes());
                out
            }
        }
    }

    /// Parses and checks a record: its magic, and for frames that the byte count matches the size.
    pub fn decode(b: &[u8; HEADER_LEN]) -> Result<Self, String> {
        let magic: &[u8; 4] = b[0..4].try_into().map_err(|_| "short header")?;
        let (h, last) = FrameHeader::fields(b);
        let len = FrameHeader::new(h.width, h.height, h.device_width, h.device_height, h.frame).map(|h| h.len);
        match magic {
            m if m == MAGIC => match len {
                Some(len) if len == last => Ok(Record::Frame(FrameHeader { len, ..h })),
                _ => Err("frame size mismatch".into()),
            },
            m if m == SHARED_MAGIC => match len {
                Some(len) => Ok(Record::Shared { header: FrameHeader { len, ..h }, slot: last }),
                None => Err("frame size mismatch".into()),
            },
            m if m == SURFACE_MAGIC => match len {
                Some(len) => Ok(Record::Surface { header: FrameHeader { len, ..h }, seq: last }),
                None => Err("frame size mismatch".into()),
            },
            m if m == RING_MAGIC => match last {
                1..=4096 => Ok(Record::Ring { path_len: last }),
                _ => Err("bad ring path length".into()),
            },
            m if m == ROTATION_MAGIC => match last {
                0..=3 => Ok(Record::Rotation(Rotation::from_quarters(last))),
                _ => Err("bad rotation".into()),
            },
            _ => Err("bad frame header".into()),
        }
    }
}

/// The shared frame ring: a file the server maps and writes, and clients `pread`. One page of
/// headers, then the slots' pixels, each slot starting on a page boundary:
///
/// - file header (64 bytes): `b"EMURING1"`, u32 version, u32 slot count, u64 slot capacity, u64
///   offset of slot 0's pixels;
/// - slot `i`'s header at `64 + 64 * i` (a [`ring::Slot`]);
/// - slot `i`'s pixels at `data_offset + i * capacity`.
///
/// A slot's sequence number is odd while the server writes it (a seqlock): a client reads the
/// header, the pixels, then the sequence number again, and keeps the pixels only if nothing moved
/// ([`ring::Slot::read_is_whole`]). The server announces each frame (`EMUS`) after writing it and
/// goes round the slots, so a client that reads promptly never races the writer; the check catches
/// one that falls a whole ring behind.
pub mod ring {
    /// The file's first bytes.
    pub const FILE_MAGIC: &[u8; 8] = b"EMURING1";
    pub const VERSION: u32 = 1;
    /// Slot alignment: Apple silicon's 16 KiB pages (and so any smaller page).
    pub const PAGE: u64 = 16 * 1024;
    pub const FILE_HEADER_LEN: usize = 64;
    pub const SLOT_HEADER_LEN: usize = 64;
    /// Slot headers that fit in the header page.
    pub const MAX_SLOTS: u32 = ((PAGE as usize - FILE_HEADER_LEN) / SLOT_HEADER_LEN) as u32;

    /// Where everything sits in a ring file.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Layout {
        pub slots: u32,
        /// Bytes each slot can hold, a whole number of pages.
        pub capacity: u64,
    }

    impl Layout {
        /// `slots` slots (at least 1, at most [`MAX_SLOTS`]) of at least `frame_bytes` each.
        pub fn new(slots: u32, frame_bytes: u64) -> Layout {
            Layout { slots: slots.clamp(1, MAX_SLOTS), capacity: frame_bytes.max(1).div_ceil(PAGE) * PAGE }
        }

        pub fn data_offset(&self) -> u64 {
            PAGE
        }

        pub fn file_len(&self) -> u64 {
            self.data_offset() + u64::from(self.slots) * self.capacity
        }

        pub fn slot_header_offset(&self, slot: u32) -> u64 {
            (FILE_HEADER_LEN + slot as usize * SLOT_HEADER_LEN) as u64
        }

        pub fn slot_data_offset(&self, slot: u32) -> u64 {
            self.data_offset() + u64::from(slot) * self.capacity
        }

        pub fn encode(&self) -> [u8; FILE_HEADER_LEN] {
            let mut out = [0u8; FILE_HEADER_LEN];
            out[0..8].copy_from_slice(FILE_MAGIC);
            out[8..12].copy_from_slice(&VERSION.to_le_bytes());
            out[12..16].copy_from_slice(&self.slots.to_le_bytes());
            out[16..24].copy_from_slice(&self.capacity.to_le_bytes());
            out[24..32].copy_from_slice(&self.data_offset().to_le_bytes());
            out
        }

        /// Parses and checks a file header: the magic, the version, and a layout this code would
        /// have written.
        pub fn decode(b: &[u8]) -> Result<Layout, String> {
            let b: &[u8; FILE_HEADER_LEN] = b.get(..FILE_HEADER_LEN).and_then(|b| b.try_into().ok()).ok_or("short ring header")?;
            if &b[0..8] != FILE_MAGIC {
                return Err("not a frame ring".into());
            }
            let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
            let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or_default());
            if u32_at(8) != VERSION {
                return Err(format!("frame ring version {} (this client reads {VERSION})", u32_at(8)));
            }
            let layout = Layout { slots: u32_at(12), capacity: u64_at(16) };
            if layout.slots == 0
                || layout.slots > MAX_SLOTS
                || layout.capacity == 0
                || !layout.capacity.is_multiple_of(PAGE)
                || u64_at(24) != PAGE
            {
                return Err("bad frame ring layout".into());
            }
            Ok(layout)
        }
    }

    /// One slot's header. `seq` is odd while the server writes the slot.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Slot {
        pub seq: u64,
        pub frame: u64,
        pub width: u32,
        pub height: u32,
        pub device_width: u32,
        pub device_height: u32,
        /// Bytes of RGBA rows in the slot.
        pub len: u32,
    }

    impl Slot {
        pub fn encode(&self) -> [u8; SLOT_HEADER_LEN] {
            let mut out = [0u8; SLOT_HEADER_LEN];
            out[0..8].copy_from_slice(&self.seq.to_le_bytes());
            out[8..16].copy_from_slice(&self.frame.to_le_bytes());
            for (i, v) in [self.width, self.height, self.device_width, self.device_height, self.len].into_iter().enumerate() {
                out[16 + i * 4..20 + i * 4].copy_from_slice(&v.to_le_bytes());
            }
            out
        }

        pub fn decode(b: &[u8]) -> Option<Slot> {
            let b = b.get(..36)?;
            let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
            let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or_default());
            Some(Slot {
                seq: u64_at(0),
                frame: u64_at(8),
                width: u32_at(16),
                height: u32_at(20),
                device_width: u32_at(24),
                device_height: u32_at(28),
                len: u32_at(32),
            })
        }

        /// Whether pixels read between this header and a second read of the sequence number
        /// (`seq_after`) are frame `frame`, whole: the slot wasn't mid-write when the header was
        /// read, wasn't rewritten before the second read, and holds that frame.
        pub fn read_is_whole(&self, seq_after: u64, frame: u64) -> bool {
            self.seq.is_multiple_of(2) && self.seq == seq_after && self.frame == frame
        }
    }
}

/// Rotary units per wheel click. The rotary's unit is a degree (stock's
/// `virtio_input_rotary.idc` sets `device.res = 57.295`, units per radian) and stock's gRPC
/// rotary input turns 15 of them per click, so one click scrolls a Wear OS list as far as on the
/// stock emulator.
pub const WHEEL_UNITS_PER_CLICK: i32 = 15;

/// The `REL_WHEEL` value for `clicks` rotary clicks.
pub fn wheel_units(clicks: i32) -> i32 {
    clicks.saturating_mul(WHEEL_UNITS_PER_CLICK)
}

/// One client → server line. `Wheel` carries rotary clicks; the server scales them with
/// [`wheel_units`].
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Hello {
        max_width: u32,
    },
    /// Frames through the shared ring ([`ring`]) rather than the socket.
    Shared,
    /// Frames on the socket (after [`Input::Shared`], when the client can't open the ring).
    Socket,
    /// Frames as the panel lies, with [`Record::Rotation`] saying how to turn them: the client
    /// turns them as it draws (on the GPU), so the server never does.
    Rotates,
    Touch {
        x: f32,
        y: f32,
        phase: Phase,
    },
    Key(String),
    Wheel(i32),
    /// Frames as IOSurfaces ([`Record::Surface`]) to the Mach port checked in under this bootstrap
    /// name (no whitespace).
    Surfaces(String),
    /// Done with surface `seq` ([`Record::Surface`]).
    Release(u32),
}

impl Input {
    /// The line, without its newline.
    pub fn to_line(&self) -> String {
        match self {
            Self::Hello { max_width } => format!("hello {max_width}"),
            Self::Shared => "shm".into(),
            Self::Socket => "socket".into(),
            Self::Rotates => "rotates".into(),
            Self::Touch { x, y, phase } => {
                let phase = match phase {
                    Phase::Down => "down",
                    Phase::Move => "move",
                    Phase::Up => "up",
                };
                format!("touch {x:.0} {y:.0} {phase}")
            }
            Self::Key(name) => format!("key {name}"),
            Self::Wheel(ticks) => format!("wheel {ticks}"),
            Self::Surfaces(name) => format!("surfaces {name}"),
            Self::Release(seq) => format!("release {seq}"),
        }
    }

    /// Parses a line; `None` for anything malformed (servers ignore those).
    pub fn parse(line: &str) -> Option<Self> {
        let w: Vec<&str> = line.split_whitespace().collect();
        Some(match w.as_slice() {
            ["hello", mw] => Self::Hello { max_width: mw.parse().unwrap_or(0) },
            ["shm"] => Self::Shared,
            ["socket"] => Self::Socket,
            ["rotates"] => Self::Rotates,
            ["touch", x, y, phase] => Self::Touch {
                x: x.parse().ok()?,
                y: y.parse().ok()?,
                phase: match *phase {
                    "down" => Phase::Down,
                    "move" => Phase::Move,
                    _ => Phase::Up,
                },
            },
            ["key", name] => Self::Key((*name).to_owned()),
            ["wheel", ticks] => Self::Wheel(ticks.parse().ok()?),
            ["surfaces", name] => Self::Surfaces((*name).to_owned()),
            ["release", seq] => Self::Release(seq.parse().ok()?),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_round_trip_and_are_checked() {
        let h = FrameHeader { width: 360, height: 640, device_width: 720, device_height: 1280, frame: 7, len: 360 * 640 * 4 };
        let bytes = h.encode();
        assert_eq!(&bytes[0..4], b"EMUF");
        assert_eq!(&bytes[12..16], &720u32.to_le_bytes());
        assert_eq!(FrameHeader::decode(&bytes), Ok(h));
        let mut bad = bytes;
        bad[28] ^= 1;
        assert_eq!(FrameHeader::decode(&bad), Err("frame size mismatch".into()));
        bad = bytes;
        bad[0] = b'X';
        assert_eq!(FrameHeader::decode(&bad), Err("bad frame header".into()));
    }

    #[test]
    fn records_round_trip_and_are_checked() {
        let header = FrameHeader::new(360, 640, 720, 1280, 9).expect("size");
        assert_eq!(header.len, 360 * 640 * 4);
        let records = [
            Record::Frame(header),
            Record::Shared { header, slot: 2 },
            Record::Ring { path_len: 40 },
            Record::Rotation(Rotation::Deg270),
            Record::Rotation(Rotation::Deg0),
            Record::Surface { header, seq: 77 },
        ];
        for r in records {
            assert_eq!(Record::decode(&r.encode()), Ok(r), "{r:?}");
        }
        let shared = Record::Shared { header, slot: 2 }.encode();
        assert_eq!(&shared[0..4], b"EMUS");
        assert_eq!(&shared[28..32], &2u32.to_le_bytes(), "the slot rides where EMUF has its byte count");
        // An old client's decoder takes only frames.
        assert_eq!(FrameHeader::decode(&shared), Err("bad frame header".into()));
        assert_eq!(FrameHeader::decode(&Record::Frame(header).encode()), Ok(header));
        let huge = Record::Shared { header: FrameHeader { width: 65536, height: 65536, ..header }, slot: 0 }.encode();
        assert_eq!(Record::decode(&huge), Err("frame size mismatch".into()));
        assert_eq!(FrameHeader::new(65536, 65536, 1, 1, 0), None);
        for bad_len in [0, 4097] {
            assert_eq!(Record::decode(&Record::Ring { path_len: bad_len }.encode()), Err("bad ring path length".into()));
        }
        let mut turned = Record::Rotation(Rotation::Deg90).encode();
        assert_eq!(&turned[0..4], b"EMUO");
        turned[28] = 4;
        assert_eq!(Record::decode(&turned), Err("bad rotation".into()));
        assert_eq!(
            FrameHeader::decode(&Record::Rotation(Rotation::Deg90).encode()),
            Err("bad frame header".into()),
            "an old client's decoder takes only frames"
        );
        let surface = Record::Surface { header, seq: 77 }.encode();
        assert_eq!(&surface[0..4], b"EMUM");
        assert_eq!(&surface[28..32], &77u32.to_le_bytes(), "the sequence number rides in the last u32");
        assert_eq!(FrameHeader::decode(&surface), Err("bad frame header".into()), "an old client's decoder takes only frames");
        assert_eq!(SURFACE_MSG_ID.to_le_bytes(), *b"EMUM");
    }

    #[test]
    fn a_send_that_failed_after_copyin_is_told_from_one_refused_before() {
        // MACH_SEND_TIMED_OUT, MACH_SEND_INTERRUPTED, MACH_SEND_INVALID_NOTIFY: pseudo-received.
        for kr in [0x1000_0004, 0x1000_0007, 0x1000_000b] {
            assert!(mach_send_pseudo_received(kr), "{kr:#x}");
        }
        assert!(mach_send_pseudo_received(0x1000_0004 | 0x2000), "with the copyout's own error bits");
        // MACH_SEND_INVALID_DEST (a dead port), MACH_SEND_INVALID_RIGHT: nothing was copied in.
        assert!(!mach_send_pseudo_received(0x1000_0003) && !mach_send_pseudo_received(0x1000_000a));
        assert!(!mach_send_pseudo_received(0), "sent");
        assert_eq!(Input::parse(&Input::Surfaces(NO_SURFACES.into()).to_line()), Some(Input::Surfaces("-".into())));
    }

    #[test]
    fn ring_layouts_are_page_aligned_and_checked() {
        let l = ring::Layout::new(3, 1080 * 2400 * 4);
        assert_eq!(l.capacity % ring::PAGE, 0);
        assert!(l.capacity >= 1080 * 2400 * 4 && l.capacity < 1080 * 2400 * 4 + ring::PAGE);
        assert_eq!(l.slot_data_offset(0), ring::PAGE);
        assert_eq!(l.slot_data_offset(2), ring::PAGE + 2 * l.capacity);
        assert_eq!(l.file_len(), ring::PAGE + 3 * l.capacity);
        assert_eq!(l.slot_header_offset(1), 128);
        assert_eq!(ring::Layout::decode(&l.encode()), Ok(l));
        assert_eq!(ring::Layout::new(0, 0), ring::Layout { slots: 1, capacity: ring::PAGE });
        assert_eq!(ring::Layout::new(10_000, 1).slots, ring::MAX_SLOTS);
        assert!(ring::Layout::new(ring::MAX_SLOTS, 1).slot_header_offset(ring::MAX_SLOTS) <= ring::PAGE);
        let mut bad = l.encode();
        bad[0] = b'X';
        assert_eq!(ring::Layout::decode(&bad), Err("not a frame ring".into()));
        let mut newer = l.encode();
        newer[8] = 2;
        assert!(ring::Layout::decode(&newer).is_err_and(|e| e.contains("version 2")));
        let mut odd = l.encode();
        odd[16] = 1;
        assert_eq!(ring::Layout::decode(&odd), Err("bad frame ring layout".into()));
        assert_eq!(ring::Layout::decode(&[0; 10]), Err("short ring header".into()));
    }

    #[test]
    fn slots_round_trip_and_reads_check_their_sequence() {
        let s = ring::Slot { seq: 4, frame: 7, width: 2, height: 3, device_width: 4, device_height: 6, len: 24 };
        assert_eq!(ring::Slot::decode(&s.encode()), Some(s));
        assert_eq!(ring::Slot::decode(&[0; 8]), None);
        assert!(s.read_is_whole(4, 7));
        assert!(!s.read_is_whole(6, 7), "rewritten while being read");
        assert!(!s.read_is_whole(4, 8), "holds another frame");
        assert!(!ring::Slot { seq: 5, ..s }.read_is_whole(5, 7), "mid-write");
    }

    #[test]
    fn input_lines_round_trip() {
        let inputs = [
            Input::Hello { max_width: 720 },
            Input::Shared,
            Input::Socket,
            Input::Rotates,
            Input::Touch { x: 10.0, y: 20.0, phase: Phase::Down },
            Input::Touch { x: 11.0, y: 21.0, phase: Phase::Move },
            Input::Touch { x: 12.0, y: 22.0, phase: Phase::Up },
            Input::Key("HOME".into()),
            Input::Wheel(-2),
            Input::Surfaces("dev.simee.surfaces.42.0".into()),
            Input::Release(7),
        ];
        for i in inputs {
            assert_eq!(Input::parse(&i.to_line()), Some(i.clone()), "{}", i.to_line());
        }
        assert_eq!(Input::Touch { x: 10.4, y: 19.6, phase: Phase::Up }.to_line(), "touch 10 20 up");
        assert_eq!(Input::parse("release x"), None);
        assert_eq!(Input::parse("surfaces"), None);
    }

    #[test]
    fn wheel_clicks_scale_to_degrees() {
        assert_eq!(wheel_units(1), 15);
        assert_eq!(wheel_units(-2), -30);
        assert_eq!(wheel_units(0), 0);
        assert_eq!(wheel_units(i32::MAX), i32::MAX, "saturates");
        assert_eq!(Input::parse("touch 1 nope up"), None);
        assert_eq!(Input::parse("dance"), None);
    }
}
