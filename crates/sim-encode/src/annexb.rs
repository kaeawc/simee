//! H.264 byte formats on the encoder side. VideoToolbox stores a picture's NAL units each behind
//! a big-endian length (the `avcC` sample format of ISO/IEC 14496-15, usually 4 bytes) and keeps
//! the SPS and PPS out of band in its format description; a WebRTC publisher (and any Annex B
//! consumer) wants start codes, with the SPS and PPS in front of every IDR so a viewer that joins
//! late or recovers from loss can decode it. [`ParameterSets`] caches the latest pair and
//! [`ParameterSets::access_unit`] builds each Annex B access unit.
//! An encoder that writes Annex B itself (OpenH264) goes through
//! [`ParameterSets::annex_b_access_unit`], which only adds what an IDR came without.

use crate::{Error, Result};

pub const START_CODE: [u8; 4] = [0, 0, 0, 1];
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// The NAL unit type (the low five bits of the header byte); 0 for an empty unit.
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// The NAL units of a length-prefixed sample (`len_size`-byte big-endian lengths), in order.
/// Zero-length units are skipped; a length that runs past the end is an error.
pub fn length_prefixed_units(sample: &[u8], len_size: u8) -> Result<Vec<&[u8]>> {
    if !(1..=4).contains(&len_size) {
        return Err(Error::InvalidInput(format!("NAL length size {len_size}")));
    }
    let n = usize::from(len_size);
    let mut out = Vec::new();
    let mut at = 0;
    while at < sample.len() {
        let Some(prefix) = sample.get(at..at + n) else {
            return Err(Error::InvalidInput(format!("a NAL length cut short at byte {at} of {}", sample.len())));
        };
        let len = prefix.iter().fold(0usize, |acc, &b| acc << 8 | usize::from(b));
        at += n;
        let Some(nal) = sample.get(at..at + len) else {
            return Err(Error::InvalidInput(format!("a {len}-byte NAL unit at byte {at} runs past the {}-byte sample", sample.len())));
        };
        if len > 0 {
            out.push(nal);
        }
        at += len;
    }
    Ok(out)
}

/// A length-prefixed sample as Annex B (4-byte start codes), appended to `out`.
pub fn length_prefixed_to_annex_b(sample: &[u8], len_size: u8, out: &mut Vec<u8>) -> Result<()> {
    for nal in length_prefixed_units(sample, len_size)? {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    Ok(())
}

/// The NAL units of an Annex B stream (3- or 4-byte start codes; trailing zeros dropped).
pub fn annex_b_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let (mut start, mut i) = (None, 0);
    let mut push = |from: usize, to: usize| {
        let nal = &stream[from..to];
        let end = nal.iter().rposition(|&b| b != 0).map_or(0, |e| e + 1);
        if end > 0 {
            out.push(&nal[..end]);
        }
    };
    while i + 2 < stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            if let Some(s) = start {
                push(s, i);
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        push(s, stream.len());
    }
    out
}

/// Annex B NAL units to a length-prefixed sample with 4-byte lengths (what a decoder that takes
/// `avcC` samples, such as VideoToolbox's, wants), leaving out SPS and PPS units.
pub fn annex_b_to_length_prefixed(stream: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(stream.len());
    for nal in annex_b_units(stream).into_iter().filter(|n| !matches!(nal_type(n), NAL_SPS | NAL_PPS)) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// The latest SPS and PPS the encoder reported (its format description's), put in front of every
/// IDR.
#[derive(Clone, Debug, Default)]
pub struct ParameterSets {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    changes: u64,
}

impl ParameterSets {
    pub fn sps(&self) -> Option<&[u8]> {
        self.sps.as_deref()
    }

    pub fn pps(&self) -> Option<&[u8]> {
        self.pps.as_deref()
    }

    /// How many times the SPS or PPS changed (a new session, a new size).
    pub fn changes(&self) -> u64 {
        self.changes
    }

    /// Takes the parameter sets a keyframe came with (the first SPS and PPS win; others are
    /// ignored). Returns whether either changed.
    pub fn update<S: AsRef<[u8]>>(&mut self, sets: &[S]) -> bool {
        let pick = |t| sets.iter().map(AsRef::as_ref).find(|s| nal_type(s) == t);
        let mut changed = false;
        for (slot, found) in [(&mut self.sps, pick(NAL_SPS)), (&mut self.pps, pick(NAL_PPS))] {
            if let Some(found) = found
                && slot.as_deref() != Some(found)
            {
                *slot = Some(found.to_vec());
                changed = true;
            }
        }
        self.changes += u64::from(changed);
        changed
    }

    /// One Annex B access unit from a length-prefixed sample. A keyframe gets the cached SPS and
    /// PPS in front unless it carries its own before its first slice; one with none cached is
    /// [`Error::InvalidInput`] (a viewer couldn't decode it).
    pub fn access_unit(&self, sample: &[u8], len_size: u8, keyframe: bool) -> Result<Vec<u8>> {
        self.assemble(&length_prefixed_units(sample, len_size)?, keyframe, sample.len())
    }

    /// One Annex B access unit from an encoder that writes Annex B itself (OpenH264): the SPS and
    /// PPS it wrote with an IDR are cached, and an IDR that came without them gets the cached
    /// ones in front. Returns the unit and whether it holds an IDR slice; an IDR with no
    /// parameter sets anywhere is [`Error::InvalidInput`].
    pub fn annex_b_access_unit(&mut self, stream: &[u8]) -> Result<(Vec<u8>, bool)> {
        let units = annex_b_units(stream);
        let keyframe = units.iter().any(|n| nal_type(n) == NAL_IDR);
        if keyframe {
            self.update(&units);
        }
        Ok((self.assemble(&units, keyframe, stream.len())?, keyframe))
    }

    fn assemble(&self, units: &[&[u8]], keyframe: bool, size_hint: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(size_hint + 64);
        let first_slice = units.iter().position(|n| (1..=5).contains(&nal_type(n)));
        let before = &units[..first_slice.unwrap_or(units.len())];
        let has = |t| before.iter().any(|n| nal_type(n) == t);
        if keyframe {
            for (t, cached) in [(NAL_SPS, &self.sps), (NAL_PPS, &self.pps)] {
                if has(t) {
                    continue;
                }
                let set = cached.as_deref().ok_or_else(|| Error::InvalidInput("a keyframe before any SPS and PPS".into()))?;
                out.extend_from_slice(&START_CODE);
                out.extend_from_slice(set);
            }
        }
        for nal in units {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(nal);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x42, 0xe0, 0x1f, 0xab];
    const PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x01];
    const P: &[u8] = &[0x41, 0x9a, 0x02];
    const SEI: &[u8] = &[0x06, 0x05, 0x01];

    fn prefixed(units: &[&[u8]], len_size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for u in units {
            out.extend_from_slice(&(u.len() as u32).to_be_bytes()[4 - len_size..]);
            out.extend_from_slice(u);
        }
        out
    }

    #[test]
    fn length_prefixed_samples_become_annex_b() {
        for len_size in [1, 2, 4] {
            let mut out = Vec::new();
            length_prefixed_to_annex_b(&prefixed(&[SEI, IDR], len_size), len_size as u8, &mut out).expect("convert");
            assert_eq!(out, [&START_CODE[..], SEI, &START_CODE, IDR].concat(), "len_size {len_size}");
        }
        // A 300-byte unit needs both bytes of a 2-byte length.
        let big = [&[0x41][..], &[0x55; 299]].concat();
        let sample = prefixed(&[&big], 2);
        assert_eq!(length_prefixed_units(&sample, 2).expect("units"), vec![&big[..]]);
        // Zero-length units are dropped; empty samples are empty.
        assert_eq!(length_prefixed_units(&[0, 0, 0, 0, 0, 0, 0, 1, 0x41], 4).expect("units"), vec![&[0x41][..]]);
        assert!(length_prefixed_units(&[], 4).expect("empty").is_empty());
    }

    #[test]
    fn malformed_samples_are_errors() {
        assert!(length_prefixed_units(&[0, 0, 0, 9, 0x41], 4).is_err(), "runs past the end");
        assert!(length_prefixed_units(&[0, 0, 0, 2, 0x41, 0x9a, 0, 0], 4).is_err(), "a cut-short length");
        assert!(length_prefixed_units(&[1, 0x41], 0).is_err() && length_prefixed_units(&[1, 0x41], 5).is_err());
    }

    #[test]
    fn annex_b_round_trips_to_length_prefixed_without_parameter_sets() {
        let annex_b = [&[0, 0, 0, 1][..], SPS, &[0, 0, 1], PPS, &[0, 0, 0, 1], IDR, &[0, 0, 1], SEI, &[0, 0]].concat();
        assert_eq!(annex_b_units(&annex_b), vec![SPS, PPS, IDR, SEI]);
        assert_eq!(annex_b_to_length_prefixed(&annex_b), prefixed(&[IDR, SEI], 4));
        assert!(annex_b_units(&[0x65, 1, 2]).is_empty(), "no start code, no units");
    }

    #[test]
    fn idrs_get_the_cached_parameter_sets_in_front_and_p_frames_dont() {
        let mut sets = ParameterSets::default();
        assert!(sets.access_unit(&prefixed(&[IDR], 4), 4, true).is_err(), "nothing cached yet");
        assert_eq!(sets.access_unit(&prefixed(&[P], 4), 4, false).expect("p"), [&START_CODE[..], P].concat());
        assert!(sets.update(&[SPS, PPS]));
        assert!(!sets.update(&[SPS.to_vec(), PPS.to_vec()]), "the same sets again");
        assert_eq!(sets.changes(), 1);
        let idr = sets.access_unit(&prefixed(&[SEI, IDR], 4), 4, true).expect("idr");
        assert_eq!(idr, [&START_CODE[..], SPS, &START_CODE, PPS, &START_CODE, SEI, &START_CODE, IDR].concat());
        // Every IDR gets them, not only the first.
        assert_eq!(
            sets.access_unit(&prefixed(&[IDR], 4), 4, true).expect("idr"),
            [&START_CODE[..], SPS, &START_CODE, PPS, &START_CODE, IDR].concat()
        );
        // An IDR that carries its own isn't given a second copy.
        let own = sets.access_unit(&prefixed(&[SPS, PPS, IDR], 4), 4, true).expect("own");
        assert_eq!(annex_b_units(&own), vec![SPS, PPS, IDR]);
        // A new SPS (a new size) replaces the cached one.
        let sps2 = [0x67, 0x4d, 0x00, 0x28];
        assert!(sets.update(&[&sps2[..], PPS]));
        assert_eq!((sets.sps(), sets.pps(), sets.changes()), (Some(&sps2[..]), Some(PPS), 2));
        assert_eq!(annex_b_units(&sets.access_unit(&prefixed(&[IDR], 4), 4, true).expect("idr")), vec![&sps2[..], PPS, IDR]);
    }

    #[test]
    fn annex_b_encoder_output_gets_parameter_sets_before_every_idr() {
        let mut sets = ParameterSets::default();
        let annex_b = |units: &[&[u8]]| units.iter().flat_map(|u| [&START_CODE[..], u].concat()).collect::<Vec<u8>>();
        // An IDR that brings its own SPS and PPS is passed through, and they are cached.
        let own = annex_b(&[SPS, PPS, IDR]);
        assert_eq!(sets.annex_b_access_unit(&own).expect("idr"), (own.clone(), true));
        assert_eq!((sets.sps(), sets.pps()), (Some(SPS), Some(PPS)));
        // A P frame is left alone, and isn't a keyframe.
        let p = annex_b(&[P]);
        assert_eq!(sets.annex_b_access_unit(&p).expect("p"), (p, false));
        // An IDR without them (an encoder that wrote them once) gets the cached pair in front.
        assert_eq!(sets.annex_b_access_unit(&annex_b(&[SEI, IDR])).expect("idr"), (annex_b(&[SPS, PPS, SEI, IDR]), true));
        // An IDR with no parameter sets cached or carried can't be decoded by a late viewer.
        assert!(ParameterSets::default().annex_b_access_unit(&annex_b(&[IDR])).is_err());
    }
}
