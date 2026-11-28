//! The framing the NAT reads and writes itself: Ethernet, ARP, IPv4, UDP and ICMP echo. TCP
//! goes through smoltcp, which needs only to see the frame.
use std::net::{Ipv4Addr, SocketAddrV4};

pub const ETH_HDR: usize = 14;
pub const IP_HDR: usize = 20;
pub const UDP_HDR: usize = 8;
pub const ICMP_HDR: usize = 8;
/// The guest's MTU: no GSO, so no IP packet in either direction is larger.
pub const MTU: usize = 1500;

const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;
pub const ICMP_ECHO_REPLY: u8 = 0;
pub const ICMP_ECHO_REQUEST: u8 = 8;

/// What a frame from the guest is, as far as the NAT cares.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    /// An ARP request (`request`) or reply about `target`.
    Arp {
        request: bool,
        target: Ipv4Addr,
    },
    Tcp {
        src: SocketAddrV4,
        dst: SocketAddrV4,
        syn: bool,
        ack: bool,
        rst: bool,
    },
    Udp {
        src: SocketAddrV4,
        dst: SocketAddrV4,
        payload: &'a [u8],
    },
    /// An echo request: the whole ICMP message (header included).
    Ping {
        src: Ipv4Addr,
        dst: Ipv4Addr,
        ident: u16,
        message: &'a [u8],
    },
    /// Anything else (IPv6, fragments, multicast, other ICMP): dropped.
    Other,
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn ip4(b: &[u8], at: usize) -> Ipv4Addr {
    Ipv4Addr::new(b[at], b[at + 1], b[at + 2], b[at + 3])
}

/// The frame's source MAC, if it has an Ethernet header.
pub fn src_mac(frame: &[u8]) -> Option<[u8; 6]> {
    frame.get(6..12).and_then(|m| m.try_into().ok())
}

pub fn parse(frame: &[u8]) -> Frame<'_> {
    if frame.len() < ETH_HDR {
        return Frame::Other;
    }
    let body = &frame[ETH_HDR..];
    match be16(frame, 12) {
        ETHERTYPE_ARP => parse_arp(body),
        ETHERTYPE_IPV4 => parse_ipv4(body),
        _ => Frame::Other,
    }
}

/// Whether a frame is link control the guest's stack needs more than any bulk data: an ARP
/// message, or a TCP segment with no payload (an ACK, SYN-ACK, FIN or RST).
pub fn is_control(frame: &[u8]) -> bool {
    if frame.len() < ETH_HDR {
        return false;
    }
    let b = &frame[ETH_HDR..];
    match be16(frame, 12) {
        ETHERTYPE_ARP => true,
        ETHERTYPE_IPV4 if b.len() >= IP_HDR && b[0] >> 4 == 4 && b[9] == PROTO_TCP => {
            let ihl = usize::from(b[0] & 0x0f) * 4;
            let total = usize::from(be16(b, 2)).min(b.len());
            match b.get(ihl..total) {
                Some(l4) if l4.len() >= 20 => l4.len() <= usize::from(l4[12] >> 4) * 4,
                _ => false,
            }
        }
        _ => false,
    }
}

/// Whether `network loss` may drop a frame: an IPv4 TCP, UDP or ICMP packet (any fragment of
/// one), either way. ARP and anything not IPv4 never are: the link itself is not lossy.
pub fn droppable(frame: &[u8]) -> bool {
    frame.len() >= ETH_HDR + IP_HDR
        && be16(frame, 12) == ETHERTYPE_IPV4
        && frame[ETH_HDR] >> 4 == 4
        && matches!(frame[ETH_HDR + 9], PROTO_TCP | PROTO_UDP | PROTO_ICMP)
}

fn parse_arp(b: &[u8]) -> Frame<'_> {
    // Ethernet/IPv4 ARP only: htype 1, ptype 0x0800, hlen 6, plen 4.
    if b.len() < 28 || be16(b, 0) != 1 || be16(b, 2) != ETHERTYPE_IPV4 || b[4] != 6 || b[5] != 4 {
        return Frame::Other;
    }
    match be16(b, 6) {
        1 => Frame::Arp { request: true, target: ip4(b, 24) },
        2 => Frame::Arp { request: false, target: ip4(b, 24) },
        _ => Frame::Other,
    }
}

fn parse_ipv4(b: &[u8]) -> Frame<'_> {
    if b.len() < IP_HDR || b[0] >> 4 != 4 {
        return Frame::Other;
    }
    let ihl = usize::from(b[0] & 0x0f) * 4;
    let total = usize::from(be16(b, 2));
    if ihl < IP_HDR || total < ihl || total > b.len() {
        return Frame::Other;
    }
    // Fragments: the guest's TCP stays under the MSS and its datagrams under the MTU in practice.
    if be16(b, 6) & 0x3fff != 0 {
        return Frame::Other;
    }
    let (src, dst) = (ip4(b, 12), ip4(b, 16));
    if !is_unicast(dst) {
        return Frame::Other;
    }
    let l4 = &b[ihl..total];
    match b[9] {
        PROTO_TCP if l4.len() >= 20 => {
            let flags = l4[13];
            Frame::Tcp {
                src: SocketAddrV4::new(src, be16(l4, 0)),
                dst: SocketAddrV4::new(dst, be16(l4, 2)),
                syn: flags & 0x02 != 0,
                rst: flags & 0x04 != 0,
                ack: flags & 0x10 != 0,
            }
        }
        PROTO_UDP if l4.len() >= UDP_HDR => {
            let len = usize::from(be16(l4, 4));
            if len < UDP_HDR || len > l4.len() {
                return Frame::Other;
            }
            Frame::Udp { src: SocketAddrV4::new(src, be16(l4, 0)), dst: SocketAddrV4::new(dst, be16(l4, 2)), payload: &l4[UDP_HDR..len] }
        }
        PROTO_ICMP if l4.len() >= ICMP_HDR && l4[0] == ICMP_ECHO_REQUEST && l4[1] == 0 => {
            Frame::Ping { src, dst, ident: be16(l4, 4), message: l4 }
        }
        _ => Frame::Other,
    }
}

fn is_unicast(a: Ipv4Addr) -> bool {
    !(a.is_broadcast() || a.is_multicast() || a.is_unspecified())
}

/// The Internet checksum's running sum over `data`, which must start at an even offset of the
/// summed message.
pub fn sum(data: &[u8], mut acc: u32) -> u32 {
    let (words, rest) = data.as_chunks::<2>();
    for w in words {
        acc += u32::from(u16::from_be_bytes(*w));
    }
    if let [last] = rest {
        acc += u32::from(*last) << 8;
    }
    acc
}

pub fn fold(mut acc: u32) -> u16 {
    while acc >> 16 != 0 {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    !(acc as u16)
}

fn pseudo_sum(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, len: usize) -> u32 {
    let acc = sum(&src.octets(), 0);
    let acc = sum(&dst.octets(), acc);
    acc + u32::from(proto) + len as u32
}

/// Fills in a UDP header in `datagram[..8]` for the payload after it.
pub fn udp_header(datagram: &mut [u8], src: SocketAddrV4, dst: SocketAddrV4) {
    let len = datagram.len();
    datagram[0..2].copy_from_slice(&src.port().to_be_bytes());
    datagram[2..4].copy_from_slice(&dst.port().to_be_bytes());
    datagram[4..6].copy_from_slice(&(len as u16).to_be_bytes());
    datagram[6..8].copy_from_slice(&[0, 0]);
    let c = fold(sum(datagram, pseudo_sum(*src.ip(), *dst.ip(), PROTO_UDP, len)));
    // 0 means "no checksum" in UDP; all ones is the same sum.
    let c = if c == 0 { 0xffff } else { c };
    datagram[6..8].copy_from_slice(&c.to_be_bytes());
}

/// Turns an ICMP message in place into an echo reply with `ident`, and checksums it.
pub fn echo_reply(message: &mut [u8], ident: u16) {
    message[0] = ICMP_ECHO_REPLY;
    message[1] = 0;
    message[2..4].copy_from_slice(&[0, 0]);
    message[4..6].copy_from_slice(&ident.to_be_bytes());
    let c = fold(sum(message, 0));
    message[2..4].copy_from_slice(&c.to_be_bytes());
}

/// The two ends of the guest's link.
#[derive(Clone, Copy)]
pub struct Link {
    pub gateway_mac: [u8; 6],
    pub guest_mac: [u8; 6],
}

/// Payload bytes per fragment: what the MTU leaves after the IP header, a multiple of 8.
const FRAGMENT: usize = (MTU - IP_HDR) & !7;

/// How many frames [`ipv4_frames`] makes of an `len`-byte IP payload.
pub fn fragments(len: usize) -> usize {
    len.div_ceil(FRAGMENT).max(1)
}

/// The most frames one datagram (at most 64 KiB) takes.
pub const MAX_FRAGMENTS: usize = (u16::MAX as usize).div_ceil(FRAGMENT);

/// Wraps an IPv4 payload (`l4`, header included) in frames for the guest, fragmented to its MTU.
pub fn ipv4_frames(out: &mut Vec<Vec<u8>>, link: Link, src: Ipv4Addr, dst: Ipv4Addr, proto: u8, id: u16, l4: &[u8]) {
    let mut offset = 0;
    loop {
        let chunk = &l4[offset..l4.len().min(offset + FRAGMENT)];
        let more = offset + chunk.len() < l4.len();
        let mut f = Vec::with_capacity(ETH_HDR + IP_HDR + chunk.len());
        f.extend_from_slice(&link.guest_mac);
        f.extend_from_slice(&link.gateway_mac);
        f.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        let flags_frag = (if more { 0x2000 } else { 0 }) | (offset / 8) as u16;
        let mut ip = [0u8; IP_HDR];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&((IP_HDR + chunk.len()) as u16).to_be_bytes());
        ip[4..6].copy_from_slice(&id.to_be_bytes());
        ip[6..8].copy_from_slice(&flags_frag.to_be_bytes());
        ip[8] = 64;
        ip[9] = proto;
        ip[12..16].copy_from_slice(&src.octets());
        ip[16..20].copy_from_slice(&dst.octets());
        let c = fold(sum(&ip, 0));
        ip[10..12].copy_from_slice(&c.to_be_bytes());
        f.extend_from_slice(&ip);
        f.extend_from_slice(chunk);
        out.push(f);
        offset += chunk.len();
        if !more {
            break;
        }
    }
}

/// The IPv4 payload of an ICMP datagram socket's read: Darwin includes the IP header, Linux
/// doesn't.
pub fn strip_ip_header(b: &[u8]) -> Option<&[u8]> {
    if b.first().is_some_and(|v| v >> 4 == 4) {
        let ihl = usize::from(b[0] & 0x0f) * 4;
        b.get(ihl..)
    } else {
        Some(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checksum_of_a_checksummed_header_is_zero() {
        let mut out = Vec::new();
        let link = Link { gateway_mac: [2; 6], guest_mac: [4; 6] };
        ipv4_frames(&mut out, link, Ipv4Addr::new(10, 0, 2, 2), Ipv4Addr::new(10, 0, 2, 15), PROTO_UDP, 7, &[0; 8]);
        assert_eq!(fold(sum(&out[0][ETH_HDR..ETH_HDR + IP_HDR], 0)), 0);
    }

    #[test]
    fn fragments_counts_the_frames_ipv4_frames_makes() {
        let link = Link { gateway_mac: [2; 6], guest_mac: [4; 6] };
        for len in [0, 1, FRAGMENT - 1, FRAGMENT, FRAGMENT + 1, 8008, usize::from(u16::MAX)] {
            let mut out = Vec::new();
            ipv4_frames(&mut out, link, Ipv4Addr::new(10, 0, 2, 2), Ipv4Addr::new(10, 0, 2, 15), PROTO_UDP, 7, &vec![0; len]);
            assert_eq!(fragments(len), out.len(), "{len} bytes");
        }
        assert_eq!(fragments(usize::from(u16::MAX)), MAX_FRAGMENTS);
    }

    #[test]
    fn odd_lengths_sum_with_a_zero_pad() {
        assert_eq!(sum(&[1, 2, 3], 0), 0x0102 + 0x0300);
    }

    #[test]
    fn ip_header_is_stripped_only_when_there_is_one() {
        let with = [0x45, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, ICMP_ECHO_REPLY, 0];
        assert_eq!(strip_ip_header(&with), Some(&[ICMP_ECHO_REPLY, 0][..]));
        assert_eq!(strip_ip_header(&[ICMP_ECHO_REPLY, 0]), Some(&[ICMP_ECHO_REPLY, 0][..]));
    }

    #[test]
    fn fragments_and_multicast_are_other() {
        let mut f = vec![0u8; ETH_HDR + IP_HDR + UDP_HDR];
        f[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        let ip = &mut f[ETH_HDR..];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&((IP_HDR + UDP_HDR) as u16).to_be_bytes());
        ip[9] = PROTO_UDP;
        ip[16..20].copy_from_slice(&[8, 8, 8, 8]);
        ip[IP_HDR + 4..IP_HDR + 6].copy_from_slice(&8u16.to_be_bytes());
        assert!(matches!(parse(&f), Frame::Udp { .. }));
        f[ETH_HDR + 6] = 0x20; // more fragments
        assert_eq!(parse(&f), Frame::Other);
        f[ETH_HDR + 6] = 0;
        f[ETH_HDR + 16..ETH_HDR + 20].copy_from_slice(&[224, 0, 0, 251]);
        assert_eq!(parse(&f), Frame::Other);
    }
}
