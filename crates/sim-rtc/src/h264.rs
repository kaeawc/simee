//! H.264 elementary streams on the send side: Annex B splitting (ITU-T H.264 Annex B), access
//! units (§7.4.1.2.3), the SPS's `profile-level-id` (RFC 6184 §8.1) and whether a stream may be
//! sent on a negotiated one, and the SPS/PPS that must precede every IDR so a viewer that joins
//! late (or recovers from loss) can decode it.

use std::fmt;

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// The NAL unit type (the low five bits of the header byte); 0 for an empty unit.
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// Coded slices (§7.4.1.2.4): what makes a picture.
pub fn is_vcl(nal: &[u8]) -> bool {
    (1..=5).contains(&nal_type(nal))
}

/// The NAL units of an Annex B byte stream, start codes (`00 00 01` or `00 00 00 01`) and
/// trailing zero bytes stripped. Bytes before the first start code count as a unit of their own,
/// so a caller that hands over one bare NAL unit still gets it back.
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + 2 < stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            push_trimmed(&mut out, &stream[start..i]);
            i += 3;
            start = i;
        } else {
            i += 1;
        }
    }
    push_trimmed(&mut out, &stream[start..]);
    out
}

fn push_trimmed<'a>(out: &mut Vec<&'a [u8]>, nal: &'a [u8]) {
    let end = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    if end > 0 {
        out.push(&nal[..end]);
    }
}

/// Split a byte stream into access units (§7.4.1.2.3): a new one starts at an access unit
/// delimiter, at an SPS, PPS or SEI after a picture's slices, and at a slice whose
/// `first_mb_in_slice` is 0 after another picture's slices. Enough for encoders that emit one
/// byte stream (x264, ffmpeg, MediaCodec); an encoder that hands over frames needn't use it.
pub fn access_units(stream: &[u8]) -> Vec<Vec<&[u8]>> {
    let mut units: Vec<Vec<&[u8]>> = Vec::new();
    let mut current: Vec<&[u8]> = Vec::new();
    let mut seen_vcl = false;
    for nal in nal_units(stream) {
        let t = nal_type(nal);
        let first_slice = is_vcl(nal) && nal.get(1).is_some_and(|b| b & 0x80 != 0);
        let starts_new = t == NAL_AUD || (seen_vcl && (matches!(t, NAL_SEI | NAL_SPS | NAL_PPS | 14..=18) || first_slice));
        if starts_new && !current.is_empty() {
            units.push(std::mem::take(&mut current));
            seen_vcl = false;
        }
        seen_vcl |= is_vcl(nal);
        current.push(nal);
    }
    if !current.is_empty() {
        units.push(current);
    }
    units
}

/// RFC 6184's `profile-level-id`: `profile_idc`, the constraint flags byte and `level_idc`, the
/// first three bytes of an SPS after its header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProfileLevelId {
    pub profile_idc: u8,
    /// `constraint_set0_flag` is the top bit.
    pub constraints: u8,
    pub level_idc: u8,
}

/// The profile families WebRTC endpoints negotiate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Baseline with `constraint_set1_flag` (`42e0xx`, `42c0xx`): what every browser decodes.
    ConstrainedBaseline,
    Baseline,
    Main,
    High,
    Other(u8),
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Profile::ConstrainedBaseline => f.write_str("Constrained Baseline"),
            Profile::Baseline => f.write_str("Baseline"),
            Profile::Main => f.write_str("Main"),
            Profile::High => f.write_str("High"),
            Profile::Other(idc) => write!(f, "profile_idc {idc}"),
        }
    }
}

impl ProfileLevelId {
    /// Constrained Baseline, level 3.1 (1280x720 at 30 fps): the WebRTC default.
    pub const CONSTRAINED_BASELINE_3_1: ProfileLevelId = ProfileLevelId::new(0x42, 0xe0, 0x1f);
    /// Main, level 3.1.
    pub const MAIN_3_1: ProfileLevelId = ProfileLevelId::new(0x4d, 0x00, 0x1f);

    pub const fn new(profile_idc: u8, constraints: u8, level_idc: u8) -> ProfileLevelId {
        ProfileLevelId { profile_idc, constraints, level_idc }
    }

    /// From an SPS NAL unit (header byte first). `None` for anything else.
    pub fn from_sps(sps: &[u8]) -> Option<ProfileLevelId> {
        match sps {
            [h, p, c, l, ..] if h & 0x1f == NAL_SPS => Some(ProfileLevelId::new(*p, *c, *l)),
            _ => None,
        }
    }

    /// From the six hex digits of an `fmtp` line.
    pub fn parse(hex: &str) -> Option<ProfileLevelId> {
        if hex.len() != 6 {
            return None;
        }
        let v = u32::from_str_radix(hex, 16).ok()?;
        Some(ProfileLevelId::from_u32(v))
    }

    pub const fn from_u32(v: u32) -> ProfileLevelId {
        ProfileLevelId::new((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    pub const fn as_u32(self) -> u32 {
        (self.profile_idc as u32) << 16 | (self.constraints as u32) << 8 | self.level_idc as u32
    }

    pub fn profile(self) -> Profile {
        match self.profile_idc {
            0x42 if self.constraints & 0x40 != 0 => Profile::ConstrainedBaseline,
            0x42 => Profile::Baseline,
            0x4d => Profile::Main,
            0x64 => Profile::High,
            other => Profile::Other(other),
        }
    }

    /// Whether a stream whose SPS says `stream` may go out on a session that negotiated `self`:
    /// the same `profile_idc` (a Baseline session takes Baseline with or without
    /// `constraint_set1`, as AutoMobile learned from Android encoders that emit `42 80 xx`) and a
    /// level no higher than negotiated.
    pub fn accepts(self, stream: ProfileLevelId) -> bool {
        self.profile_idc == stream.profile_idc && level_rank(stream) <= level_rank(self)
    }
}

/// Levels in order. Level 1b is `level_idc` 11 with `constraint_set3_flag` in Baseline, Main
/// and Extended (A.3.1), or `level_idc` 9 elsewhere: it sits between 1 and 1.1.
fn level_rank(p: ProfileLevelId) -> u16 {
    let one_b = p.level_idc == 9 || (p.level_idc == 11 && matches!(p.profile_idc, 0x42 | 0x4d | 0x58) && p.constraints & 0x10 != 0);
    if one_b { 10 * 2 + 1 } else { u16::from(p.level_idc) * 2 }
}

impl fmt::Display for ProfileLevelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}{:02x}{:02x}", self.profile_idc, self.constraints, self.level_idc)
    }
}

/// The latest SPS and PPS a stream sent, so each IDR can go out with them in front even when the
/// encoder only emitted them once (VideoToolbox keeps them in its format description; x264 by
/// default only at the start of the stream).
#[derive(Clone, Debug, Default)]
pub struct ParameterSets {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParameterSets {
    pub fn sps(&self) -> Option<&[u8]> {
        self.sps.as_deref()
    }

    pub fn pps(&self) -> Option<&[u8]> {
        self.pps.as_deref()
    }

    /// Remember this access unit's SPS and PPS (the last of each wins).
    pub fn observe(&mut self, nals: &[&[u8]]) {
        for nal in nals {
            match nal_type(nal) {
                NAL_SPS => self.sps = Some(nal.to_vec()),
                NAL_PPS => self.pps = Some(nal.to_vec()),
                _ => {}
            }
        }
    }

    /// The units to send for an access unit already [`observe`](Self::observe)d: access unit
    /// delimiters dropped (RFC 6184 receivers don't need them), and for an IDR, the cached SPS
    /// and PPS in front of it when the unit doesn't carry them before its first slice.
    /// `None` for an IDR when no SPS or PPS has been seen yet.
    pub fn assemble<'a>(&'a self, nals: &[&'a [u8]]) -> Option<Vec<&'a [u8]>> {
        let first_vcl = nals.iter().position(|n| is_vcl(n));
        let idr = first_vcl.is_some_and(|i| nal_type(nals[i]) == NAL_IDR);
        let mut out = Vec::with_capacity(nals.len() + 2);
        if idr {
            let before = &nals[..first_vcl.unwrap_or(0)];
            let has = |t| before.iter().any(|n| nal_type(n) == t);
            if !has(NAL_SPS) {
                out.push(self.sps.as_deref()?);
            }
            if !has(NAL_PPS) {
                out.push(self.pps.as_deref()?);
            }
        }
        out.extend(nals.iter().copied().filter(|n| nal_type(n) != NAL_AUD));
        Some(out)
    }
}

/// Whether an access unit starts a decodable picture (its first slice is an IDR).
pub fn is_keyframe(nals: &[&[u8]]) -> bool {
    nals.iter().find(|n| is_vcl(n)).is_some_and(|n| nal_type(n) == NAL_IDR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b_with_both_start_codes() {
        let s = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 4, 5, 0, 0];
        assert_eq!(nal_units(&s), vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]);
        assert_eq!(nal_units(&[0x65, 1, 2]), vec![&[0x65, 1, 2][..]]);
        assert!(nal_units(&[0, 0, 0, 1]).is_empty());
        assert!(nal_units(&[]).is_empty());
    }

    #[test]
    fn parses_and_prints_profile_level_ids() {
        let cb = ProfileLevelId::from_sps(&[0x67, 0x42, 0xc0, 0x1e, 0xda]).expect("sps");
        assert_eq!((cb.to_string(), cb.profile()), ("42c01e".to_owned(), Profile::ConstrainedBaseline));
        assert_eq!(ProfileLevelId::parse("42e01f"), Some(ProfileLevelId::CONSTRAINED_BASELINE_3_1));
        assert_eq!(ProfileLevelId::parse("4d001f").map(|p| p.profile()), Some(Profile::Main));
        assert_eq!(ProfileLevelId::parse("42001f").map(|p| p.profile()), Some(Profile::Baseline));
        assert_eq!(ProfileLevelId::parse("640c2a").map(|p| p.profile()), Some(Profile::High));
        assert_eq!(ProfileLevelId::from_u32(0x42e01f).as_u32(), 0x42e01f);
        assert_eq!(ProfileLevelId::parse("42e01"), None);
        assert_eq!(ProfileLevelId::parse("zze01f"), None);
        assert_eq!(ProfileLevelId::from_sps(&[0x68, 0x42, 0xc0, 0x1e]), None, "a PPS");
        assert_eq!(ProfileLevelId::from_sps(&[0x67, 0x42]), None, "truncated");
    }

    #[test]
    fn a_session_accepts_its_own_profile_at_or_below_its_level() {
        let cb31 = ProfileLevelId::CONSTRAINED_BASELINE_3_1;
        let main31 = ProfileLevelId::MAIN_3_1;
        assert!(cb31.accepts(ProfileLevelId::parse("42c01e").expect("x264 baseline")));
        assert!(cb31.accepts(ProfileLevelId::parse("42801f").expect("android baseline")));
        assert!(main31.accepts(ProfileLevelId::parse("4d401f").expect("x264 main")));
        assert!(!cb31.accepts(ProfileLevelId::parse("4d401f").expect("main")), "Main on a Baseline session");
        assert!(!main31.accepts(ProfileLevelId::parse("42e01f").expect("cb")), "Baseline on a Main session");
        assert!(!cb31.accepts(ProfileLevelId::parse("64001f").expect("high")));
        assert!(!cb31.accepts(ProfileLevelId::parse("42e028").expect("level 4")), "above the level");
        // Level 1b sits between 1 and 1.1.
        let l1b = ProfileLevelId::new(0x42, 0xf0, 11);
        assert!(l1b.accepts(ProfileLevelId::new(0x42, 0xe0, 10)));
        assert!(!l1b.accepts(ProfileLevelId::new(0x42, 0xe0, 11)));
    }

    #[test]
    fn idrs_get_the_cached_parameter_sets_in_front() {
        let (sps, pps, idr, p, aud) = (&[0x67, 0x42][..], &[0x68, 1][..], &[0x65, 0x88][..], &[0x41, 0x9a][..], &[0x09, 0xf0][..]);
        let mut sets = ParameterSets::default();
        sets.observe(&[idr]);
        assert_eq!(sets.assemble(&[idr]), None, "no SPS seen yet");
        assert_eq!(sets.assemble(&[aud, p]), Some(vec![p]), "a P frame needs none; AUD dropped");
        sets.observe(&[sps, pps, idr]);
        assert_eq!(sets.assemble(&[sps, pps, idr]), Some(vec![sps, pps, idr]), "already there");
        sets.observe(&[aud, idr]);
        assert_eq!(sets.assemble(&[aud, idr]), Some(vec![sps, pps, idr]), "both inserted, AUD dropped");
        assert_eq!(sets.assemble(&[pps, idr]), Some(vec![sps, pps, idr]), "only the SPS was missing");
        assert!(is_keyframe(&[sps, pps, idr]) && !is_keyframe(&[p]) && !is_keyframe(&[sps]));
    }

    #[test]
    fn groups_units_into_access_units() {
        let s: Vec<u8> = [
            &[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1e][..],
            &[0, 0, 0, 1, 0x68, 0xce],
            &[0, 0, 1, 0x06, 0x05],
            &[0, 0, 1, 0x65, 0x88, 0x84],
            &[0, 0, 1, 0x41, 0x9a, 0x01],
            &[0, 0, 1, 0x41, 0x40, 0x02], // a second slice of the same picture (first_mb != 0)
            &[0, 0, 0, 1, 0x09, 0xf0],
            &[0, 0, 1, 0x41, 0x9a, 0x03],
        ]
        .concat();
        let units = access_units(&s);
        let types: Vec<Vec<u8>> = units.iter().map(|u| u.iter().map(|n| nal_type(n)).collect()).collect();
        assert_eq!(types, vec![vec![7, 8, 6, 5], vec![1, 1], vec![9, 1]]);
    }
}
