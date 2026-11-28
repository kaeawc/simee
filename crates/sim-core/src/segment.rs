//! `net-segment`: planning one L2 segment between linked Android devices (#539).
//!
//! Each Android guest sits alone behind its own NAT (10.0.2.0/24), so linked devices can't see
//! each other and multicast (mDNS, Nearby's Wi-Fi LAN medium) never leaves the guest. A segment
//! gives every member a second NIC on a link subnet, and emu relays that NIC's frames between the
//! members' processes over Unix datagram sockets in one directory (`emu-vmm`'s `net::segment`).
//! The NAT and `eth0` are untouched, so unlinked devices are unaffected.
//!
//! This module is the pure part: which subnet, which address and MAC each member gets, where the
//! members' sockets live, the `-net-segment` argument that tells emu, and the commands that set the
//! guest's side up. Nothing here touches a file or a socket.
//!
//! Addresses are derived, not stored: member `k` of subnet `n` is `10.77.n.(k+1)` with MAC
//! `52:55:0a:4d:n:(k+1)` (the MAC spells the IP, as slirp's does), so a relay can route a
//! unicast frame to the member that owns its destination MAC without learning anything.

use std::fmt;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// The first two octets of every segment's subnet. Not 10.0.2.0/24 (the NAT) and away from the
/// ranges home routers and VPNs hand out.
pub const NET_PREFIX: [u8; 2] = [10, 77];
/// Subnet numbers `1..=254` (`10.77.n.0/24`).
pub const MAX_SUBNETS: usize = 254;
/// Members per segment: host numbers `.1` to `.16`.
pub const MAX_MEMBERS: usize = 16;
/// The prefix length every member configures.
pub const PREFIX_LEN: u8 = 24;
/// The guest's name for the segment NIC: the NAT's `eth0` stays first (the radio HAL configures
/// that name), so the segment's is the second.
pub const GUEST_IFACE: &str = "eth1";
/// The longest socket path a member may have: `sun_path` holds 104 bytes on macOS, with its NUL.
pub const MAX_SOCKET_PATH: usize = 100;
/// The longest segment name (it is a directory name).
pub const MAX_NAME: usize = 32;

/// Whether `name` is usable as a segment's directory name: lowercase letters, digits, `-` and
/// `_`, starting with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= MAX_NAME
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// The lowest subnet number (`10.77.n.0/24`) not in `taken`, or `None` when all 254 are.
pub fn allocate_subnet(taken: &[u8]) -> Option<u8> {
    (1..=254u8).find(|n| !taken.contains(n))
}

/// The lowest member index not in `taken`, or `None` when the segment is full.
pub fn allocate_member(taken: &[u8]) -> Option<u8> {
    (0..MAX_MEMBERS as u8).find(|k| !taken.contains(k))
}

/// Member `member`'s address on `subnet`.
pub fn ip_for(subnet: u8, member: u8) -> Ipv4Addr {
    Ipv4Addr::new(NET_PREFIX[0], NET_PREFIX[1], subnet, member + 1)
}

/// Member `member`'s MAC on `subnet`: locally administered, unicast, and the address in bytes.
pub fn mac_for(subnet: u8, member: u8) -> [u8; 6] {
    [0x52, 0x55, NET_PREFIX[0], NET_PREFIX[1], subnet, member + 1]
}

/// The member that owns `mac` on `subnet`, if it is one of this scheme's MACs.
pub fn member_of_mac(mac: [u8; 6], subnet: u8) -> Option<u8> {
    let ours = mac[..5] == mac_for(subnet, 0)[..5];
    (ours && (1..=MAX_MEMBERS as u8).contains(&mac[5])).then(|| mac[5] - 1)
}

/// Whether `mac` is a group address (broadcast or multicast): the low bit of its first byte.
pub fn is_group(mac: &[u8]) -> bool {
    mac.first().is_some_and(|b| b & 1 == 1)
}

/// The file name of member `member`'s socket in the segment's directory.
pub fn socket_name(member: u8) -> String {
    format!("m{member}.sock")
}

/// The member a socket file name (`m3.sock`) belongs to.
pub fn member_of_socket(name: &str) -> Option<u8> {
    let n: u8 = name.strip_prefix('m')?.strip_suffix(".sock")?.parse().ok()?;
    // `m03.sock` isn't ours: one name per member.
    (usize::from(n) < MAX_MEMBERS && socket_name(n) == name).then_some(n)
}

/// What one emu process needs to join a segment: the `-net-segment` argument.
///
/// Text form `dir=PATH,subnet=N,member=K`; the directory holds every member's socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentSpec {
    pub dir: PathBuf,
    pub subnet: u8,
    pub member: u8,
}

impl SegmentSpec {
    /// Checks the pieces: a subnet and member this scheme can address, and a socket path that
    /// fits `sun_path`.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=254).contains(&self.subnet) {
            return Err(format!("subnet {} is outside 1..=254", self.subnet));
        }
        if usize::from(self.member) >= MAX_MEMBERS {
            return Err(format!("member {} is outside 0..={}", self.member, MAX_MEMBERS - 1));
        }
        let dir = self.dir.to_str().ok_or("the segment directory isn't UTF-8")?;
        if dir.is_empty() || dir.contains(',') {
            return Err(format!("the segment directory '{dir}' is empty or has a comma"));
        }
        let longest = dir.len() + 1 + socket_name(MAX_MEMBERS as u8 - 1).len();
        if longest > MAX_SOCKET_PATH {
            return Err(format!(
                "a socket path in {dir} would be {longest} bytes; unix sockets allow {MAX_SOCKET_PATH} here (use a shorter directory)"
            ));
        }
        Ok(())
    }

    pub fn ip(&self) -> Ipv4Addr {
        ip_for(self.subnet, self.member)
    }

    pub fn mac(&self) -> [u8; 6] {
        mac_for(self.subnet, self.member)
    }

    /// This member's own socket.
    pub fn socket(&self) -> PathBuf {
        self.dir.join(socket_name(self.member))
    }

    /// Member `member`'s socket in the same segment.
    pub fn socket_of(&self, member: u8) -> PathBuf {
        self.dir.join(socket_name(member))
    }
}

impl fmt::Display for SegmentSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dir={},subnet={},member={}", self.dir.display(), self.subnet, self.member)
    }
}

impl FromStr for SegmentSpec {
    type Err = String;

    fn from_str(text: &str) -> Result<SegmentSpec, String> {
        let (mut dir, mut subnet, mut member) = (None, None, None);
        for part in text.split(',') {
            let (key, value) = part.split_once('=').ok_or_else(|| format!("'{part}' isn't key=value"))?;
            let number = |what: &str| value.parse::<u8>().map_err(|_| format!("{what} '{value}' isn't a number"));
            match key {
                "dir" => dir = Some(PathBuf::from(value)),
                "subnet" => subnet = Some(number("subnet")?),
                "member" => member = Some(number("member")?),
                other => return Err(format!("unknown key '{other}' (expected dir, subnet, member)")),
            }
        }
        let spec = SegmentSpec {
            dir: dir.ok_or("missing dir=")?,
            subnet: subnet.ok_or("missing subnet=")?,
            member: member.ok_or("missing member=")?,
        };
        spec.validate()?;
        Ok(spec)
    }
}

/// One member of a planned segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedMember {
    /// What the caller calls the device (an AVD name).
    pub name: String,
    pub spec: SegmentSpec,
}

impl PlannedMember {
    /// The commands (run as root in the guest: `adb root`, then `adb shell`) that bring the
    /// segment NIC up with its address and multicast. See [`guest_setup`].
    pub fn guest_setup(&self) -> Vec<String> {
        guest_setup(self.spec.ip())
    }
}

/// The commands that configure the guest's side of the segment NIC.
///
/// Linux's side only: the NIC needs its address, `MULTICAST`, and a route for 224.0.0.0/4 (the
/// guest's default route is the NAT's, which would send mDNS there). Whether Android's
/// connectivity stack then offers the interface to `NsdManager` is not settled without a
/// booted guest (`docs/design/net-segment.md`, "Not verified").
pub fn guest_setup(ip: Ipv4Addr) -> Vec<String> {
    vec![
        format!("ip addr add {ip}/{PREFIX_LEN} dev {GUEST_IFACE}"),
        format!("ip link set {GUEST_IFACE} multicast on up"),
        format!("ip route add 224.0.0.0/4 dev {GUEST_IFACE}"),
    ]
}

/// A segment: a name, a directory under `base`, a subnet and its members.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentPlan {
    pub name: String,
    pub dir: PathBuf,
    pub subnet: u8,
    pub members: Vec<PlannedMember>,
}

impl SegmentPlan {
    /// A segment named `name` with its directory under `base` and the lowest subnet not in
    /// `taken_subnets` (those of the segments that already exist).
    pub fn new(name: &str, base: &Path, taken_subnets: &[u8]) -> Result<SegmentPlan, String> {
        if !valid_name(name) {
            return Err(format!("'{name}' isn't a segment name (lowercase letters, digits, - and _; at most {MAX_NAME})"));
        }
        let subnet = allocate_subnet(taken_subnets).ok_or("all 254 segment subnets are in use")?;
        let plan = SegmentPlan { name: name.to_string(), dir: base.join(name), subnet, members: Vec::new() };
        SegmentSpec { dir: plan.dir.clone(), subnet, member: 0 }.validate()?;
        Ok(plan)
    }

    /// A segment of `names`, in order: member 0 first.
    pub fn of(name: &str, base: &Path, taken_subnets: &[u8], names: &[&str]) -> Result<SegmentPlan, String> {
        if names.len() < 2 {
            return Err("a segment links at least two devices".into());
        }
        let mut plan = SegmentPlan::new(name, base, taken_subnets)?;
        for n in names {
            plan.add(n)?;
        }
        Ok(plan)
    }

    /// Adds a device on the lowest free member index; an index freed by [`SegmentPlan::remove`]
    /// is reused, and nobody else's address changes.
    pub fn add(&mut self, device: &str) -> Result<&PlannedMember, String> {
        if self.members.iter().any(|m| m.name == device) {
            return Err(format!("{device} is already in segment {}", self.name));
        }
        let taken: Vec<u8> = self.members.iter().map(|m| m.spec.member).collect();
        let member = allocate_member(&taken).ok_or_else(|| format!("segment {} is full ({MAX_MEMBERS} members)", self.name))?;
        self.members
            .push(PlannedMember { name: device.to_string(), spec: SegmentSpec { dir: self.dir.clone(), subnet: self.subnet, member } });
        Ok(self.members.last().expect("just pushed"))
    }

    /// Removes a device; false when it wasn't a member.
    pub fn remove(&mut self, device: &str) -> bool {
        let before = self.members.len();
        self.members.retain(|m| m.name != device);
        self.members.len() != before
    }

    pub fn member(&self, device: &str) -> Option<&PlannedMember> {
        self.members.iter().find(|m| m.name == device)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PathBuf {
        PathBuf::from("/tmp/simee/seg")
    }

    #[test]
    fn names_are_directory_safe() {
        for ok in ["pair", "xr-1", "a_b", "0day"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "-x", "_x", "A", "a/b", "a b", "a.b", "..", &"x".repeat(MAX_NAME + 1)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(valid_name(&"x".repeat(MAX_NAME)));
    }

    #[test]
    fn subnets_are_the_lowest_free_and_run_out() {
        assert_eq!(allocate_subnet(&[]), Some(1));
        assert_eq!(allocate_subnet(&[1, 2, 4]), Some(3));
        assert_eq!(allocate_subnet(&[2]), Some(1));
        let all: Vec<u8> = (1..=254).collect();
        assert_eq!(allocate_subnet(&all), None);
        assert_eq!(allocate_subnet(&all[1..]), Some(1));
    }

    #[test]
    fn addresses_and_macs_follow_the_member() {
        assert_eq!(ip_for(3, 0), Ipv4Addr::new(10, 77, 3, 1));
        assert_eq!(ip_for(3, 15), Ipv4Addr::new(10, 77, 3, 16));
        assert_eq!(mac_for(3, 1), [0x52, 0x55, 10, 77, 3, 2]);
        // Never the NAT's address space, and never the group bit.
        assert_ne!(ip_for(1, 0).octets()[..3], [10, 0, 2]);
        for s in [1, 254] {
            for k in 0..MAX_MEMBERS as u8 {
                assert!(!is_group(&mac_for(s, k)));
                assert_eq!(member_of_mac(mac_for(s, k), s), Some(k));
            }
        }
    }

    #[test]
    fn a_mac_belongs_to_its_own_subnet_and_scheme_only() {
        assert_eq!(member_of_mac(mac_for(3, 2), 4), None);
        assert_eq!(member_of_mac([0x52, 0x54, 0, 0x12, 0x34, 0x56], 3), None, "the NAT NIC's MAC");
        assert_eq!(member_of_mac([0x52, 0x55, 10, 77, 3, 0], 3), None);
        assert_eq!(member_of_mac([0x52, 0x55, 10, 77, 3, 17], 3), None);
        assert_eq!(member_of_mac([0xff; 6], 3), None);
    }

    #[test]
    fn group_addresses_are_broadcast_and_multicast() {
        assert!(is_group(&[0xff; 6]));
        assert!(is_group(&[0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb]), "mDNS, IPv4");
        assert!(is_group(&[0x33, 0x33, 0, 0, 0, 0xfb]), "mDNS, IPv6");
        assert!(!is_group(&mac_for(1, 0)));
        assert!(!is_group(&[]));
    }

    #[test]
    fn socket_names_round_trip_and_reject_lookalikes() {
        for k in 0..MAX_MEMBERS as u8 {
            assert_eq!(member_of_socket(&socket_name(k)), Some(k));
        }
        for bad in ["m16.sock", "m03.sock", "m.sock", "m1.sock.tmp", "x1.sock", "m-1.sock", "segment.json"] {
            assert_eq!(member_of_socket(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_spec_argument_round_trips() {
        let spec = SegmentSpec { dir: base().join("pair"), subnet: 7, member: 1 };
        let text = spec.to_string();
        assert_eq!(text, "dir=/tmp/simee/seg/pair,subnet=7,member=1");
        assert_eq!(text.parse::<SegmentSpec>(), Ok(spec.clone()));
        assert_eq!(spec.ip(), Ipv4Addr::new(10, 77, 7, 2));
        assert_eq!(spec.mac(), mac_for(7, 1));
        assert_eq!(spec.socket(), base().join("pair/m1.sock"));
        assert_eq!(spec.socket_of(0), base().join("pair/m0.sock"));
    }

    #[test]
    fn a_bad_spec_says_what_is_wrong() {
        let err = |s: &str| s.parse::<SegmentSpec>().expect_err("refused");
        assert!(err("dir=/x,subnet=1").contains("missing member"));
        assert!(err("subnet=1,member=0").contains("missing dir"));
        assert!(err("dir=/x,subnet=0,member=0").contains("subnet 0"));
        assert!(err("dir=/x,subnet=1,member=16").contains("member 16"));
        assert!(err("dir=/x,subnet=300,member=0").contains("isn't a number"));
        assert!(err("dir=/x,subnet=1,member=0,speed=3").contains("unknown key 'speed'"));
        assert!(err("dir=/x,subnet=1,member").contains("key=value"));
        assert!(err("dir=,subnet=1,member=0").contains("empty"));
        let long = format!("dir=/{},subnet=1,member=0", "d".repeat(95));
        assert!(err(&long).contains("shorter directory"));
    }

    #[test]
    fn a_plan_gives_each_member_its_own_address() {
        let plan = SegmentPlan::of("pair", &base(), &[1, 2], &["phone", "headset"]).expect("plan");
        assert_eq!(plan.subnet, 3);
        assert_eq!(plan.dir, base().join("pair"));
        let ips: Vec<_> = plan.members.iter().map(|m| m.spec.ip()).collect();
        assert_eq!(ips, [Ipv4Addr::new(10, 77, 3, 1), Ipv4Addr::new(10, 77, 3, 2)]);
        assert_eq!(plan.member("headset").map(|m| m.spec.member), Some(1));
        assert!(plan.members.iter().all(|m| m.spec.validate().is_ok()));
    }

    #[test]
    fn a_plan_refuses_what_cannot_be_a_segment() {
        let of = |names: &[&str]| SegmentPlan::of("pair", &base(), &[], names).expect_err("refused");
        assert!(of(&["alone"]).contains("at least two"));
        assert!(of(&["a", "a"]).contains("already in segment"));
        assert!(SegmentPlan::new("Bad Name", &base(), &[]).expect_err("refused").contains("isn't a segment name"));
        let all: Vec<u8> = (1..=254).collect();
        assert!(SegmentPlan::new("pair", &base(), &all).expect_err("refused").contains("254"));
        let many: Vec<String> = (0..=MAX_MEMBERS).map(|i| format!("d{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(of(&many).contains("full"));
        let deep = PathBuf::from(format!("/{}", "d".repeat(95)));
        assert!(SegmentPlan::new("pair", &deep, &[]).expect_err("refused").contains("shorter directory"));
    }

    #[test]
    fn adding_and_removing_leaves_the_others_where_they_are() {
        let mut plan = SegmentPlan::of("pair", &base(), &[], &["a", "b", "c"]).expect("plan");
        let c = plan.member("c").expect("c").clone();
        assert!(plan.remove("b"));
        assert!(!plan.remove("b"));
        assert_eq!(plan.member("c"), Some(&c), "c keeps its address");
        // The freed index is the next one handed out.
        assert_eq!(plan.add("d").expect("d").spec.member, 1);
        assert_eq!(plan.add("e").expect("e").spec.member, 3);
    }

    #[test]
    fn the_guest_setup_addresses_the_second_nic_and_routes_multicast_to_it() {
        let plan = SegmentPlan::of("pair", &base(), &[], &["a", "b"]).expect("plan");
        let cmds = plan.member("b").expect("b").guest_setup();
        assert_eq!(cmds, ["ip addr add 10.77.1.2/24 dev eth1", "ip link set eth1 multicast on up", "ip route add 224.0.0.0/4 dev eth1"]);
        assert!(cmds.iter().all(|c| !c.contains("eth0")), "the NAT NIC is left alone");
    }
}
