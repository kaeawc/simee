//! A screenshot sized for an agent: what `screenshot` answers (base64 PNG), shrunk so it costs a
//! model's context a few hundred tokens instead of a few thousand. The method's own answer stays
//! full size; a front end that wants the small one (MCP's `screen` resource) calls this on it.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// The longest side an agent-sized screen has, in pixels. A 1206x2622 iPhone becomes 402x874.
pub const AGENT_MAX_SIDE: u32 = 1024;

/// `png` (base64) shrunk by a whole factor until its longest side is at most `max_side`; as it is
/// when it already is.
pub fn shrink_png_base64(png: &str, max_side: u32) -> Result<String, String> {
    let bytes = STANDARD.decode(png).map_err(|e| format!("the screenshot isn't base64: {e}"))?;
    let small = sim_core::frame::shrink_png(&bytes, max_side)?;
    Ok(if small == bytes { png.to_owned() } else { STANDARD.encode(small) })
}

#[cfg(test)]
mod tests {
    use sim_core::frame::{Frame, decode_png, encode_png};

    use super::*;

    fn png(width: u32, height: u32) -> String {
        STANDARD.encode(encode_png(&Frame { width, height, rgba: vec![0x80; width as usize * height as usize * 4] }).expect("png"))
    }

    #[test]
    fn a_phone_screen_is_cut_to_the_agent_size_and_a_small_one_is_left() {
        let big = shrink_png_base64(&png(1206, 2622), AGENT_MAX_SIDE).expect("shrink");
        let frame = decode_png(&STANDARD.decode(big).expect("base64")).expect("png");
        assert_eq!((frame.width, frame.height), (402, 874));
        let small = png(200, 300);
        assert_eq!(shrink_png_base64(&small, AGENT_MAX_SIDE), Ok(small));
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        assert!(shrink_png_base64("***", 10).expect_err("base64").contains("base64"));
        assert!(shrink_png_base64("iVBOR", 10).is_err());
    }
}
