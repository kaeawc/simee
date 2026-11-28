# net-segment: one L2 segment between linked Android devices

Issue [#539](https://github.com/kaeawc/simee/issues/539), part of the device pairing epic #429.
From the headset research ([research/headset-pairing.md](../research/headset-pairing.md)) and the
`network` mechanism in [device-links.md](device-links.md) ("A shared virtual network").

Each Android guest sits alone behind emu's user-mode NAT (10.0.2.0/24): it has no inbound
connection and its multicast never leaves the guest, so mDNS (`NsdManager`) and Nearby's Wi-Fi LAN
medium don't cross between devices. `net-segment` gives every linked Android member a second NIC on
a link subnet and relays that NIC's frames between the members' emu processes, multicast included.
It serves every `network` link, not only headsets.

## Status

| Part | State |
| --- | --- |
| Planning: subnet, member addresses and MACs, socket paths, the `-net-segment` argument (`sim_core::segment`) | built, unit tested |
| The relay: one backend per member over Unix datagram sockets (`emu_vmm::net::segment`) | built, tested with several members in one process |
| The second NIC: `virtio-net` at mmio slot 15, `eth1` (`emu -net-segment`) | built, DT order and MAC tested; **not booted** |
| Guest configuration (address, multicast route) | the commands are planned (`SegmentPlan::guest_setup`); **not run on a guest** |
| The link adapter that plans a segment, starts members with the flag and runs the guest commands | waits for the link model's core (#427, with #535's adapters); `net-segment` is the mechanism name it will use |
| Idle cost | thread wakeups pinned at zero by a test; CPU and memory per device **not measured** (`docs/benchmarks.md`, "Not yet measured") |

## What was compared

| Option | Why not (or why) |
| --- | --- |
| QEMU's `-netdev socket,mcast=` bus | the shape wanted (every process on one bus), but it is QEMU's and emu is not QEMU; on macOS binding to the group address is refused (EADDRNOTAVAIL) per [vagrant-qemu's notes](https://github.com/ppggff/vagrant-qemu), and the Mac is emu's main host |
| QEMU's `-netdev dgram` with Unix paths | the same transport this uses (one datagram per frame, a path per member), but point to point: more than two members need a switch process between them ([QEMU 7.2 write-up](https://john-millikin.com/improved-unix-socket-networking-in-qemu-7.2)) |
| A switch daemon between emu processes | extra process to start, supervise and stop, and a single point whose death takes the segment down |
| Stock's `-wifi-server-port` / `-wifi-client-port` | one pair of emulators, a Wi-Fi forward rather than a NIC on a subnet; a report on a TV and a phone found no discovery over it even with `-feature -WiFiPacketStream` ([kaeawc/auto-mobile#18792](https://github.com/kaeawc/auto-mobile/issues/18792)) |
| Stock's own shared network (emulator 36.5) | Google's new stack gives AVDs on a host a shared virtual network with Wi-Fi Direct and NSD ([announcement](https://daily.dev/posts/now-multiple-android-emulator-devices-can-communicate-with-each-other-out-of-the-box-using-network--t0xzhmlwu)); it is stock's, not reachable from emu's VMM, which is why a segment is needed here |
| **Unix datagram sockets in a per-link directory, one per member** | chosen, as `device-links.md` proposed: no extra process, no root, no entitlement, works on macOS and Linux, a restarting member rejoins by binding its socket |

Android side: an extra Ethernet interface is only used if the framework tracks it
(`EthernetTracker` takes interfaces matching `config_ethernet_iface_regex`, with capabilities from
`config_ethernet_interfaces`; see the
[NXP dual-Ethernet note](https://community.nxp.com/t5/i-MX-Processors-Knowledge-Base/Dual-ethernet-on-Android-14-i-MX8-MP/ta-p/1893152)
and [`EthernetTracker.java`](https://android.googlesource.com/platform/frameworks/opt/net/ethernet/+/refs/tags/android-9.0.0_r1/java/com/android/server/ethernet/EthernetTracker.java)),
and those are resource overlays of a system image, which simee does not modify.

## How it works

- **One directory per segment**, mode `0700`: `<base>/<name>/m<k>.sock`, member `k` in `0..16`.
  Each emu process binds its own socket; every datagram is one Ethernet frame. The relay trusts
  whoever can write the directory, and swapping a parent for another user's is as good as writing
  the directory, so emu checks the whole path (`net::segment::secure_dir`), as ssh's StrictModes
  does for a key: the path is resolved once and that resolved path is used from then on; every
  parent up to `/` must be owned by this user or root and not writable by group or others (group
  write is refused outright, since a group like macOS's `staff` is shared; a root-owned sticky `/tmp`
  is the one exception); the segment directory must be this user's, not a link or a file,
  and **not writable by group or others** (refused as found, not chmodded: sockets planted while it was
  open would stay; one that others can only read is set to `0700`); and every entry in it must be
  this user's. The default base is private to the user (`sim_core::segment::default_root`:
  `$XDG_RUNTIME_DIR/simee/seg`, else `~/.simee/run/seg`), where there is nothing to refuse. A base
  of `/tmp/...` works when `/tmp/...` is made by the user.
- **Addresses are derived**: member `k` of subnet `n` is `10.77.n.(k+1)/24` with MAC
  `52:55:0a:4d:n:(k+1)`. The subnet is the lowest free of 254; the NAT's 10.0.2.0/24 is untouched.
- **Routing** (`net::segment::route`): a frame to a MAC that a member owns goes to that member; a
  group address (broadcast, multicast: mDNS is `01:00:5e:00:00:fb` and `33:33:00:00:00:fb`) or a MAC
  nobody owns goes to every other member. Nothing is learned. A frame to a member that isn't running
  is dropped, like a cable that isn't plugged in. Frames from the receiver's own MAC are dropped
  on arrival (no loops).
- **Cost model**: one thread per member, blocked in `poll(2)` on its socket and a wakeup (no timer),
  so an idle member does no work; sends are non-blocking on the device's worker thread, so a slow
  peer loses its own frames and never stalls the guest or another peer. The peer list is the
  directory listing, re-read at most once a second and only when a group frame is sent.
- **The NIC** is a second `virtio-net` at mmio slot 15 (`layout::slots::SEGMENT`; the board's slots
  0–15 are otherwise empty). Linux names NICs in registration order, and the radio HAL configures
  `eth0`, so the board describes slot 15 *after* every other slot (`BoardInfo::late_slot`) whenever
  a segment is joined. Without `-net-segment` the device tree is byte for byte what it was.
- **Command line**: `emu -avd NAME -net-segment dir=PATH,subnet=N,member=K`. `-dry-run` shows the
  `segment` line. Software mode (x86_64 Linux under QEMU) refuses it.

## Defaults taken

Where the issue left a choice, the simplest fit with the existing code:

1. **Hub, not switch**: flood unknown unicast; route known (derived) MACs directly. No learning table.
2. **Directory of sockets, no daemon**, as `device-links.md` says.
3. **Subnet 10.77.n.0/24**, lowest free, derived addresses; no DHCP, no gateway (L2 only).
4. **At most 16 members** per segment, 254 segments.
5. **Socket path limit**: 100 bytes (macOS's `sun_path`), checked when the segment is planned and again on the resolved path (`/tmp` is `/private/tmp` on a Mac), so a
   long directory is refused with a reason instead of failing at boot.
6. **MTU 1500**: macOS's `net.local.dgram.maxdgram` is 2048 by default, so jumbo frames aren't relayed.
7. **Apple silicon only for now**; software mode refuses the flag.
8. **Where segments live**: `$XDG_RUNTIME_DIR/simee/seg`, else `~/.simee/run/seg`, both per user; emu accepts any path that passes its checks.
9. **Guest address by command, not by image change**: simee doesn't edit system images.

## Not verified (`needs-device`)

Nothing here was booted: the VM it was written on has no KVM and no Mac.

- That the guest's `virtio_net` binds slot 15 and names it `eth1`, with the NAT's NIC as `eth0`
  (the DT order is tested; the kernel's probe order is reasoned from `of_platform_populate`).
- That `ip addr add`, multicast on, and the 224.0.0.0/4 route (`SegmentPlan::guest_setup`, run as
  root over adb) work on the image's toybox/iproute2.
- **That the segment's routes are consulted at all.** Android's netd policy routing has no rule that
  looks up the `main` table for an interface it doesn't manage, so the connected route that
  `ip addr add` makes could be skipped and unicast between members fall through to the NAT's
  `eth0` (link-local mDNS sent with `IP_MULTICAST_IF` skips the FIB and may work regardless).
  `guest_setup` adds two `ip rule ... lookup main` lines for the subnet and 224.0.0.0/4; whether they
  are enough is a check on a guest.
- **That Android offers `eth1` to `NsdManager` and Nearby.** A Linux interface with an address is
  not an Android `Network`: without `EthernetTracker` managing it (an overlay emu can't ship), the
  connectivity stack may ignore it. This is the open risk of the whole design; if it holds, the
  fallback is a framework-visible interface (a different transport), not more relay work.
- The done-when check from the issue: two booted guests see each other's mDNS services and reach
  each other's addresses on the segment, unlinked devices unaffected (the code adds no device
  unless the flag is given).
- Idle cost per member (host CPU, memory) against a device without a segment.
