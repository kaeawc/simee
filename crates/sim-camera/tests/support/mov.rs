//! A `video:` clip for the end-to-end tests, written in the test: a QuickTime movie of uncompressed
//! 24-bit RGB frames (`raw `), built box by box. No fixture is checked in, no encoder runs and no
//! `unsafe` is needed; and since nothing is compressed, a frame decodes to the pixels written but for
//! colour matching's rounding (255 reads 254), so a frame can be compared pixel by pixel.
//!
//! Shared by `sim-camera`'s and `emu-vmm`'s end-to-end tests (`#[path]`), which only compile it
//! where `video:` plays (macOS).
#![allow(dead_code, reason = "each test uses its own part of it")]

use std::path::{Path, PathBuf};

// The including test's own imports: `sim-camera` is `simee_camera` in one and `sim_camera` in the
// other.
use crate::{CameraFrame, Tick};

/// The clip's size: 4:3, as every test camera is, so filling a camera's frame crops nothing.
pub const WIDTH: u32 = 64;
pub const HEIGHT: u32 = 48;

/// The clip's rate: each frame shows for `1000 / FPS` ms.
pub const FPS: u32 = 10;

/// The frames' colours (RGB), in order: frame `i` is `COLOURS[i]` over its top half and grey below,
/// so a frame decoded upside down or with its channels swapped compares unequal.
pub const COLOURS: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]];

const GREY: [u8; 3] = [128, 128, 128];

/// How long one frame shows.
pub fn interval_ns() -> u64 {
    1_000_000_000 / u64::from(FPS)
}

/// Frame `i` of the clip, as RGB rows.
fn rgb(i: usize) -> Vec<u8> {
    (0..HEIGHT).flat_map(|y| if y < HEIGHT / 2 { COLOURS[i] } else { GREY }.repeat(WIDTH as usize)).collect()
}

/// Frame `i` of the clip as a camera shows it at `width`x`height` (filled the way every source is).
pub fn expected(i: usize, width: u32, height: u32) -> CameraFrame {
    let bgra: Vec<u8> = rgb(i).as_chunks::<3>().0.iter().flat_map(|p| [p[2], p[1], p[0], 255]).collect();
    CameraFrame { width: WIDTH, height: HEIGHT, bgra: bgra.into(), tick: Tick::start() }.fit(width, height)
}

/// How far a decoded channel may be from the one written: AVFoundation's colour matching rounds
/// (255 decodes as 254). Far below the distance between any two of the clip's colours and grey.
const ROUNDING: u8 = 4;

/// Which clip frame `f` is, if every pixel is that frame's, drawn at `f`'s size, within
/// [`ROUNDING`].
pub fn which(f: &CameraFrame) -> Option<usize> {
    (0..COLOURS.len()).find(|&i| expected(i, f.width, f.height).bgra.iter().zip(f.bgra.iter()).all(|(a, b)| a.abs_diff(*b) <= ROUNDING))
}

/// A movie file the test wrote, removed when dropped (a failing test too).
pub struct Clip(pub PathBuf);

impl Drop for Clip {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn atom(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = 8 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&u32::try_from(len).expect("atom size").to_be_bytes());
    out.extend_from_slice(kind);
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be16(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}

/// Two atom types spelled with an escape, since the spell checker reads them as typos: an alias
/// data reference (here, "the media is in this file") and a track.
const ALIAS: &[u8; 4] = b"\x61lis";
const TRACK: &[u8; 4] = b"\x74rak";

/// The identity transform: 16.16 fixed point, with 2.30 for the last column.
const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn matrix() -> Vec<u8> {
    MATRIX.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// Writes the clip (every frame of [`COLOURS`] once, at [`FPS`]) to `/tmp/<name>-<pid>.mov`.
pub fn write(name: &str) -> Clip {
    let clip = Clip(Path::new("/tmp").join(format!("{name}-{}.mov", std::process::id())));
    let frames = COLOURS.len() as u32;
    let frame_bytes = WIDTH * HEIGHT * 3;
    let timescale = 1000;
    let delta = timescale / FPS;
    let duration = delta * frames;

    let ftyp = atom(b"ftyp", &[b"qt  ", &be32(0), b"qt  "]);
    let pixels: Vec<u8> = (0..COLOURS.len()).flat_map(rgb).collect();
    let mdat = atom(b"mdat", &[&pixels]);
    let chunk_offset = u32::try_from(ftyp.len() + 8).expect("offset");

    let zero4 = be32(0);
    let mvhd = atom(
        b"mvhd",
        &[
            &zero4,
            &zero4,
            &zero4,
            &be32(timescale),
            &be32(duration),
            &be32(0x0001_0000),
            &be16(0x0100),
            &[0; 10],
            &matrix(),
            &[0; 24],
            &be32(2),
        ],
    );
    let tkhd = atom(
        b"tkhd",
        &[
            &be32(3), // enabled, in the movie
            &zero4,
            &zero4,
            &be32(1),
            &zero4,
            &be32(duration),
            &[0; 8],
            &be16(0),
            &be16(0),
            &be16(0),
            &be16(0),
            &matrix(),
            &be32(WIDTH << 16),
            &be32(HEIGHT << 16),
        ],
    );
    let mdhd = atom(b"mdhd", &[&zero4, &zero4, &zero4, &be32(timescale), &be32(duration), &be16(0), &be16(0)]);
    let hdlr = |class: &[u8; 4], sub: &[u8; 4]| atom(b"hdlr", &[&zero4, class, sub, &zero4, &zero4, &zero4, &[0]]);
    let vmhd = atom(b"vmhd", &[&be32(1), &be16(0x0040), &[0x80, 0, 0x80, 0, 0x80, 0]]);
    let dinf = atom(b"dinf", &[&atom(b"dref", &[&zero4, &be32(1), &atom(ALIAS, &[&be32(1)])])]);
    let mut compressor = [0u8; 32];
    compressor[0] = 4;
    compressor[1..5].copy_from_slice(b"none");
    let sample = atom(
        b"raw ",
        &[
            &[0; 6],
            &be16(1), // data reference 1
            &be16(0),
            &be16(0),
            &zero4,
            &zero4,
            &zero4,
            &be16(WIDTH as u16),
            &be16(HEIGHT as u16),
            &be32(72 << 16),
            &be32(72 << 16),
            &zero4,
            &be16(1),
            &compressor,
            &be16(24),
            &be16(0xFFFF), // no colour table
        ],
    );
    let stbl = atom(
        b"stbl",
        &[
            &atom(b"stsd", &[&zero4, &be32(1), &sample]),
            &atom(b"stts", &[&zero4, &be32(1), &be32(frames), &be32(delta)]),
            &atom(b"stsc", &[&zero4, &be32(1), &be32(1), &be32(frames), &be32(1)]),
            &atom(b"stsz", &[&zero4, &be32(frame_bytes), &be32(frames)]),
            &atom(b"stco", &[&zero4, &be32(1), &be32(chunk_offset)]),
        ],
    );
    let minf = atom(b"minf", &[&vmhd, &hdlr(b"dhlr", ALIAS), &dinf, &stbl]);
    let mdia = atom(b"mdia", &[&mdhd, &hdlr(b"mhlr", b"vide"), &minf]);
    let moov = atom(b"moov", &[&mvhd, &atom(TRACK, &[&tkhd, &mdia])]);

    std::fs::write(&clip.0, [ftyp, mdat, moov].concat()).expect("write the clip");
    clip
}
