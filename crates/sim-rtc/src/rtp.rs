//! RFC 6184 H.264 RTP payloads, packetization mode 1: a NAL unit that fits the MTU goes as a
//! single NAL unit packet (§5.6), a larger one as FU-A fragments (§5.8); the marker bit is set on
//! the last packet of an access unit (§5.1). The depacketizer is the inverse (plus STAP-A,
//! §5.7.1) for tests and receivers. RTP headers, sequence numbers and SRTP are str0m's.

use std::time::Duration;

use crate::{Error, Result};

/// Leaves room under a 1500-byte Ethernet MTU for IP, UDP, RTP, header extensions and SRTP
/// (the size Chromium and AutoMobile use).
pub const DEFAULT_MTU: usize = 1200;
/// H.264's RTP clock (RFC 6184 §8.2.1).
pub const CLOCK_RATE: u32 = 90_000;

const FU_A: u8 = 28;
const STAP_A: u8 = 24;
const FU_START: u8 = 0x80;
const FU_END: u8 = 0x40;

/// One RTP payload and whether it ends its access unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payload {
    pub data: Vec<u8>,
    pub marker: bool,
}

/// Packetize one access unit's NAL units (header byte first, no start codes).
pub fn packetize(nals: &[&[u8]], mtu: usize) -> Result<Vec<Payload>> {
    if mtu <= 2 {
        return Err(Error::InvalidInput(format!("an RTP MTU of {mtu} leaves no room for an FU-A fragment's 2 header bytes")));
    }
    let mut out = Vec::new();
    for nal in nals.iter().filter(|n| !n.is_empty()) {
        packetize_nal(nal, mtu, &mut out);
    }
    if let Some(last) = out.last_mut() {
        last.marker = true;
    }
    Ok(out)
}

fn packetize_nal(nal: &[u8], mtu: usize, out: &mut Vec<Payload>) {
    if nal.len() <= mtu {
        out.push(Payload { data: nal.to_vec(), marker: false });
        return;
    }
    // The FU indicator keeps the forbidden bit and NRI; the FU header carries the type.
    let indicator = (nal[0] & 0xe0) | FU_A;
    let kind = nal[0] & 0x1f;
    let body = &nal[1..];
    let chunk = mtu - 2;
    let count = body.len().div_ceil(chunk);
    for (i, piece) in body.chunks(chunk).enumerate() {
        let mut header = kind;
        if i == 0 {
            header |= FU_START;
        }
        if i == count - 1 {
            header |= FU_END;
        }
        let mut data = Vec::with_capacity(2 + piece.len());
        data.extend_from_slice(&[indicator, header]);
        data.extend_from_slice(piece);
        out.push(Payload { data, marker: false });
    }
}

/// Reassembles NAL units from RTP payloads in sequence order. A lost FU-A fragment drops that
/// unit (the next start fragment resets).
#[derive(Debug, Default)]
pub struct Depacketizer {
    fu: Option<Vec<u8>>,
}

impl Depacketizer {
    /// The NAL units this payload completes.
    pub fn push(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>> {
        let bad = |m: &str| Err(Error::InvalidInput(format!("RTP H.264 payload: {m}")));
        let Some(&first) = payload.first() else {
            return bad("empty");
        };
        match first & 0x1f {
            1..=23 => {
                self.fu = None;
                Ok(vec![payload.to_vec()])
            }
            STAP_A => {
                self.fu = None;
                let mut out = Vec::new();
                let mut rest = &payload[1..];
                while !rest.is_empty() {
                    let [a, b, tail @ ..] = rest else {
                        return bad("truncated STAP-A size");
                    };
                    let len = usize::from(u16::from_be_bytes([*a, *b]));
                    if len == 0 || tail.len() < len {
                        return bad("truncated STAP-A unit");
                    }
                    out.push(tail[..len].to_vec());
                    rest = &tail[len..];
                }
                Ok(out)
            }
            FU_A => {
                let Some(&header) = payload.get(1) else {
                    return bad("truncated FU-A");
                };
                if header & FU_START != 0 {
                    let mut nal = vec![(first & 0xe0) | (header & 0x1f)];
                    nal.extend_from_slice(&payload[2..]);
                    self.fu = Some(nal);
                } else if let Some(nal) = &mut self.fu {
                    nal.extend_from_slice(&payload[2..]);
                } else {
                    // A middle or end fragment whose start was lost.
                    return Ok(Vec::new());
                }
                if header & FU_END != 0 {
                    return Ok(self.fu.take().into_iter().collect());
                }
                Ok(Vec::new())
            }
            t => bad(&format!("unsupported packet type {t}")),
        }
    }
}

/// An RTP timestamp: `base` plus `pts` on the 90 kHz clock, wrapping as RTP does.
pub fn timestamp(base: u32, pts: Duration) -> u32 {
    let ticks = pts.as_nanos() * u128::from(CLOCK_RATE) / 1_000_000_000;
    base.wrapping_add(ticks as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(kind: u8, len: usize) -> Vec<u8> {
        let mut v = vec![0x60 | kind];
        v.extend((0..len - 1).map(|i| (i % 251) as u8));
        v
    }

    fn round_trip(nals: &[&[u8]], mtu: usize) -> (Vec<Payload>, Vec<Vec<u8>>) {
        let payloads = packetize(nals, mtu).expect("packetize");
        let mut d = Depacketizer::default();
        let out = payloads.iter().flat_map(|p| d.push(&p.data).expect("depacketize")).collect();
        (payloads, out)
    }

    #[test]
    fn small_units_go_whole_and_the_marker_ends_the_unit() {
        let (sps, pps, slice) = (nal(7, 20), nal(8, 4), nal(1, 900));
        let (payloads, out) = round_trip(&[&sps, &pps, &slice], DEFAULT_MTU);
        assert_eq!(payloads.len(), 3);
        assert_eq!(payloads.iter().map(|p| p.marker).collect::<Vec<_>>(), [false, false, true]);
        assert_eq!(out, vec![sps, pps, slice]);
    }

    #[test]
    fn large_units_fragment_as_fu_a_within_the_mtu() {
        let idr = nal(5, 5000);
        let (payloads, out) = round_trip(&[&idr], DEFAULT_MTU);
        assert_eq!(payloads.len(), 5000_usize.div_ceil(DEFAULT_MTU - 2));
        assert!(payloads.iter().all(|p| p.data.len() <= DEFAULT_MTU));
        assert_eq!(payloads[0].data[..2], [0x60 | 28, 0x80 | 5], "indicator keeps NRI; start bit");
        assert_eq!(payloads[1].data[1], 5, "middle: no start or end bit");
        assert_eq!(payloads.last().map(|p| (p.data[1], p.marker)), Some((0x40 | 5, true)));
        assert_eq!(out, vec![idr]);
        // Exactly the MTU goes whole; one byte over splits into two fragments.
        assert_eq!(packetize(&[&nal(5, DEFAULT_MTU)], DEFAULT_MTU).expect("fit").len(), 1);
        assert_eq!(packetize(&[&nal(5, DEFAULT_MTU + 1)], DEFAULT_MTU).expect("split").len(), 2);
        // A tiny MTU still works, one body byte per fragment.
        let (payloads, out) = round_trip(&[&nal(5, 10)], 3);
        assert_eq!((payloads.len(), out), (9, vec![nal(5, 10)]));
    }

    #[test]
    fn refuses_an_mtu_without_room_and_bad_payloads() {
        assert!(packetize(&[&nal(5, 10)], 2).is_err());
        assert!(packetize(&[], DEFAULT_MTU).expect("empty").is_empty());
        let mut d = Depacketizer::default();
        assert!(d.push(&[]).is_err());
        assert!(d.push(&[28]).is_err());
        assert!(d.push(&[24, 0, 9, 1]).is_err());
        assert!(d.push(&[29, 0x85, 1]).is_err(), "FU-B is not mode 1");
        assert!(d.push(&[28, 0x45, 1]).expect("orphan end").is_empty(), "end without start dropped");
    }

    #[test]
    fn depacketizes_stap_a() {
        let mut d = Depacketizer::default();
        let stap = [24, 0, 2, 0x67, 1, 0, 3, 0x68, 2, 3];
        assert_eq!(d.push(&stap).expect("stap"), vec![vec![0x67, 1], vec![0x68, 2, 3]]);
    }

    #[test]
    fn timestamps_run_on_90_khz_from_the_pts_and_wrap() {
        assert_eq!(timestamp(0, Duration::from_secs(1)), 90_000);
        assert_eq!(timestamp(1000, Duration::from_millis(16)), 1000 + 1440);
        assert_eq!(timestamp(u32::MAX - 10, Duration::from_millis(1)), 79);
        assert_eq!(timestamp(0, Duration::from_micros(16_667)), 1500);
    }
}
