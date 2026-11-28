# Device links

One model for devices that work together: a watch and its phone, a car and a phone, a headset and
a phone, or a few devices on one network. This is the design for #427 (epic #429). The platform
work behind it is in #423 (iOS watch pairs), #424 (Wear OS), #425 (cars), #426 (headsets) and
#428 (the desktop UX); each of those builds an adapter for this model and keeps its own research.

What exists today and what this builds on:

| Mechanism | Where | What the link model takes from it |
| --- | --- | --- |
| Camera links saved with the device | `crates/sim-android/src/lib.rs` (`CAMERAS_FILE`, `save_cameras`, `update_cameras`), `crates/sim-api/src/camera.rs` | The file per device, its lock, temp-file-and-rename writes, bad lines skipped with a warning, reapplied on boot |
| simee state beside an AVD | `crates/emu-avd/src/lib.rs` (`state_dir`: `<avd>/simee/`) | Where an Android device's links live |
| simee state in a simulator | `crates/sim-api/src/camera.rs` (`<data>/simee/`, `ios_booted`, `ios_stopped`) | Where an iOS device's links live, and the boot and stop hooks |
| A device feeding another (`device:` camera source, #325) | `crates/sim-api/src/camera_device.rs` | The precedent for one device depending on another that can shut down |
| Admission on lifecycle calls | `crates/sim-api/src/methods.rs` (`admitted`, `judge_now`, `boot_many_admitted`) | Judging several devices as one request |
| Portable bundles (#169/#170) | `crates/emu-avd/src/bundle.rs` (`allowed_path`), `crates/sim-bundle/` | What an export carries and leaves out |
| Method table, effects, toolsets | `crates/sim-api/src/methods.rs` (`METHODS`), `crates/sim-api/src/catalog.rs` (`effect`, `Group`) | Link methods are API methods; CLI and MCP get them for free |
| MCP resources | `crates/sim-mcp/src/resources.rs` | A link-state resource read through `links` |
| emu's Bluetooth controller | `crates/emu-vmm/src/bt/` and [research/bluetooth.md](../research/bluetooth.md) | A controller with no peers; the netsimd bridge is the planned next step |
| emu's user-mode NAT | `crates/emu-vmm/src/net/mod.rs`, [android.md](../android.md) (Network row) | Each Android guest is alone at 10.0.2.15; the host's loopback is 10.0.2.2; no inbound `redir` yet |
| Form factors and sub-platforms | `crates/sim-core/src/form_factor.rs`, `crates/sim-core/src/platform.rs` | Roles and kind inference |

## The link

A link is a record with:

- `id`: `lnk-` and 8 hex digits, made at `link` time. For an iOS watch pair it is CoreSimulator's
  pair UUID instead, so the two never disagree.
- `kind`: `watch-phone`, `car-phone`, `headset-phone` or `network`.
- `members`: two or more devices, each `{platform, device, role}`. `device` is the stable id the
  platform uses (the AVD name, the simulator UDID); `role` is `phone`, `watch`, `car`, `headset`
  or `peer`.
- `mechanism`: what carries it on this host, chosen by the adapter (`coresim-pair`,
  `adb-forward`, `bt-netsim`, `net-forward`, `net-segment`; see the mappings below).
- `params`: per-mechanism settings that must survive a restart (forwarded ports, the Bluetooth
  addresses the members had when they bonded).
- `state`, `reason`, `since`: the last state seen, why, and when it changed.
- `rev`: a counter raised on every write, so two copies can be reconciled.

### Kinds and members

| Kind | Members | Platforms | Example |
| --- | --- | --- | --- |
| `watch-phone` | exactly one `phone` and one `watch` | one platform: iOS + watchOS, or Android + Wear OS | Apple Watch Series 10 paired to iPhone 17 |
| `car-phone` | exactly one `phone` and one `car` | Android only (an Android Automotive AVD and a phone AVD) | AAOS head unit with a Pixel for calls and media |
| `headset-phone` | exactly one `phone` and one `headset` | one platform (visionOS + iOS, Android XR + Android) | Vision Pro with an iPhone on one network |
| `network` | 2 to 16 `peer`s | any, mixed platforms allowed | an iPad, an iPhone and a Pixel testing local discovery |

Rules, checked before anything changes (a pure function in `sim-core`, like `Kind::new`):

- The role comes from the form factor: `phone` (and `tablet`, `foldable` for `network` only),
  `watch`, `auto` as `car`, `vision` as `headset` (`crates/sim-core/src/form_factor.rs`). A pair
  whose form factors don't fit the kind is refused, naming what each member is.
- `kind` may be left out when the form factors decide it: watch + phone is `watch-phone`, auto +
  phone is `car-phone`, vision + phone is `headset-phone`; anything else needs `kind: network`.
- Only `network` crosses platforms. An iPhone with a Wear OS watch is refused with the reason
  (neither simulator stack can carry it).
- A phone may be in several `watch-phone` links (CoreSimulator allows several watches per phone,
  `maxPairs`; so does Wear OS); at most one is `active` per phone on iOS. A watch is in at most
  one `watch-phone` link. A device may be in any number of `network` links.
- Android Auto through the DHU and iOS CarPlay are not links: each is one phone with a projected
  screen, not two devices (see the car mapping and open question 5).

### States

```
            link (both members exist, kinds fit)
                          │
                          ▼
   ┌───────────────► pending ◄──────────────┐
   │                   │   ▲                │
   │   adapter says    │   │ a member       │ link again
   │   established     │   │ shut down      │ (re-pair)
   │                   ▼   │                │
   │                  active ──────────► broken
   │                          member gone,  │
   │                          adapter error,│
   │                          copies differ │
   │                                        │
   └──────────── unlink (any state) ────────┴──► removed
```

- **pending**: recorded but not carrying anything yet. A member is shut down (`reason`: "the
  watch is shut down"), or the adapter's handshake hasn't finished (Wear OS pairing in progress).
  Shutting a member down is normal use, so it is pending, not broken.
- **active**: the adapter reports the link carrying traffic: CoreSimulator's pair connected, a
  Bluetooth bond with an ACL connection, the forwards listening.
- **broken**: it can't work until someone acts. A member was deleted outside simee, the adapter
  failed (`reason` carries its error), a member's copy of the record differs and can't be
  reconciled, or a member changed so the kind no longer fits (a runtime changed under a pair).
  `link` with the same members (or `link {link: <id>}`) tries again; `unlink` removes it.

The stored state is the last one seen. Every read re-derives it: `links` from the files and
whether each member runs (cheap: no device I/O), `link_status` from the adapter's probe as well.

### Persistence

Each member keeps a copy of every link it is in, in a file beside its camera links:

- Android: `<avd>/simee/links.json` (`emu_avd::state_dir`). It is simee's file, not stock's, as
  AGENTS.md requires.
- iOS: `<data>/simee/links.json`, the simulator's data directory, as `cameras.ini` is
  (`crates/sim-api/src/camera.rs`). For a watch pair CoreSimulator's own record
  (`device_set.plist`, `DevicePairs`: pair UUID → `gizmo`, `companion`, `active`) is the truth;
  simee's copy adds the fields CoreSimulator doesn't have and is rewritten from it on read.

The file is JSON (`{"links": [<link>, ...]}`, each link the API's link object below plus `rev`
and `params`), not `key=value` lines like `cameras.ini`: a link has a member list and nested
params, and `serde_json` is already a dependency of every crate involved. The Wear OS research in
PR #447 (`docs/research/wearos-pairing.md`) arrived at the same file name, `links.json`.

```json
{"links": [{"id": "lnk-3f9a2c41", "kind": "watch-phone", "rev": 4, "mechanism": "adb-forward",
  "members": [{"platform": "android", "device": "pixel-9", "role": "phone"},
              {"platform": "android", "device": "wear-5", "role": "watch"}],
  "params": {"phone_port": 5602}, "state": "active", "since": "2026-11-28T10:41:07Z"}]}
```

Writing and reading:

- A change writes every member's copy, each under that file's lock (`CamerasLock`'s pattern: a
  `.lock` file, stale after 30 s), taken in sorted member order so two changes can't deadlock,
  each written to a temp file and renamed (`write_cameras`). The code moves into one shared
  helper for both files rather than a copy.
- A copy whose own device isn't among its `members` is ignored and removed on the next
  write. That is what makes clones safe: an iOS `clone` copies `<data>/`, links included, and the
  clone is not a member. (An Android clone copies only `CLONED_DISKS`, so no file comes along.)
- Copies that disagree: the higher `rev` wins and is written back to the others. A member whose
  copy is missing (written before a crash) gets it back the same way.
- A link that doesn't parse is skipped with a warning naming the file and the link, and the rest
  still load (as `saved_cameras_warnings` does for lines).
- iOS `erase` wipes `<data>/`; simee's `erase` reads the device's `links.json` first and writes it
  back after (`crates/sim-api/src/ios.rs`, the `"shutdown" | "erase"` arm), so erasing a watch
  keeps it paired.

Nothing about a link lives only in a simee process. The parts that run do so in the device
processes or platform daemons: CoreSimulator, the adb server (forwards), emu (a link segment),
netsimd. A server restart loses nothing; the next read re-derives the state.

## Per-platform mappings

Each mapping is an adapter with one interface (below). Statements marked "to verify" come from
symbols or documentation, not from a run here, and are the device checks for the platform issue.

### iOS: CoreSimulator device pairs (`watch-phone`, #423)

CoreSimulator has pairs built in. The symbols in
`/Library/Developer/PrivateFrameworks/CoreSimulator.framework` (read with `strings`, Xcode on
this host, 2026-11-28; no class-dump headers are in the repo):

| Need | API |
| --- | --- |
| Check before pairing | `-[SimDeviceSet testDevicePairingBetweenGizmo:companion:error:]`, `+[SimDevicePair testPossiblePairingWithDeviceTypeA:RuntimeA:DeviceTypeB:RuntimeB:error:]`, `SimRuntimePairingRequirements` |
| Create | `-[SimDeviceSet createDevicePairWithGizmo:companion:error:]` (and `...Async...completionHandler:`) |
| List | `-[SimDeviceSet devicePairs]`, `availableDevicePairs`, `devicePairsContainingDevice:`, `devicePairsContainingDeviceUDID:` |
| A pair's fields | `gizmo`, `companion` (`SimDevice`), `active`, `connected`, `UUID` |
| Make active | `-[SimDevicePair setActiveWithError:]` |
| Remove | `-[SimDeviceSet unpairDevicePair:error:]` |
| Changes | XPC notifications `DevicePairAdded`, `DevicePairRemoved`, `PairConnectionStateChanged`, `PairSetActive` |
| Transport | `-[SimDevice(PairingSupport) connectIDSRelayToDevice:disconnectMonitorPort:error:]`, `setActiveIDSRelayDevice:error:`: an IDS relay between the booted devices, which is what WatchConnectivity rides |

`xcrun simctl pair <watch> <phone>`, `pair_activate`, `unpair` and `list pairs` are the public
front of the same calls. simee calls them in-process: a new `crates/sim-coresim/src/pair.rs`, each
selector checked with `respondsToSelector:` first so a missing one is `Error::Unavailable`
(AGENTS.md), next to `createDeviceWithType:runtime:name:error:` in `service.rs`.

- `mechanism`: `coresim-pair`. `active` maps to the record's `active`; `connected` with both
  booted maps to `active` state; otherwise `pending`.
- The pair survives a server restart and a host reboot on its own (it is in `device_set.plist`).
- To verify: the log strings "Error shutting down device during unpairing" and "Error erasing
  device during unpairing" suggest `unpair` shuts down and erases the watch. If it does, `unlink`
  is classed `Destroy` and says so in its result; if not, `Set`.
- To verify: whether the IDS relay needs the phone booted first. The adapter boots the phone
  first regardless (boot ordering below).
- Pairing a watch to a phone it isn't paired to, with `maxPairs` reached: CoreSimulator refuses
  ("Failed to pair ... (pairs: %lu max: %lu)"); the error is passed through as the refusal.

### Wear OS through emu (`watch-phone`, #424)

The Wear OS research (PR #447, `docs/research/wearos-pairing.md`, not merged yet) finds that
Android Studio's pairing assistant uses TCP through adb, not Bluetooth: `adb -s <phone> forward
tcp:<port> tcp:5601` and `adb -s <watch> reverse tcp:5601 tcp:<port>` (ports from 5602), a
broadcast to the watch, then the phone's companion app started on its emulator activity, which
logs the pairing state. The phone needs API 30+ with Google Play; a Google sign-in and a consent
screen on the phone may stay manual. Two mechanisms:

1. **`adb-forward`** (the default, #444/#445): the forward and the reverse above. Needs nothing
   new in emu if `forward` and `reverse` work over emu's adb bridge (#442 checks that on a
   device). The adb server holds both, so they outlive simee; after a boot of either member or an
   adb server restart, `member_booted` (or the next `link_status`) re-adds them. Host ports are
   shared by every device, so the adapter picks a free one and saves it in `params`. The link
   stays `pending`, with the companion's reported state as `reason`, until the phone logs the
   watch connected. Wear OS has no "active watch" switch, so `active` is ignored on Android.
2. **`bt-netsim`** (later): a real Bluetooth link, not needed for Wear OS pairing but required for
   AAOS below. emu's controller answers the stack but has no peers (`crates/emu-vmm/src/bt/mod.rs`:
   "Neither radio has peers yet"). The next step in [research/bluetooth.md](../research/bluetooth.md)
   (option 2) bridges the `bluetooth` port's H4 to netsimd's raw HCI port, so emu devices see and
   bond with each other through rootcanal. netsimd ships in the SDK (`emulator/netsimd`).

Bluetooth addresses come from `bt::address_for`, a hash of the AVD's disk paths. A clone or an
import gets a new address, which breaks a bond kept in user data. The adapter therefore saves
each member's address in `params` when it bonds, and emu takes it on boot (a new
`-bt-addr` flag, one line in `machine.rs` where `address_for` is called) so a moved member
keeps its bond.

### Android Auto through the DHU (#425)

The phone runs the app and Google's Desktop Head Unit (in the SDK at
`extras/google/auto/desktop-head-unit`) shows it, over `adb forward tcp:5277 tcp:5277` once the
phone's Android Auto head-unit server runs. The DHU is a host program with its own window, not an
emulator, so there is no second device to link. This design treats it as a projection of the
phone (like `carplay` on iOS, `crates/sim-coresim/src/carplay.rs`): a phone-side method that #425
defines, not a `car-phone` link. If the owner wants the DHU listed and linked as a device, it
becomes a `car` member of platform `android` with no AVD, and the store needs no change (open
question 5).

### Android Automotive with a phone (`car-phone`, #425)

The car is an AAOS AVD (`vehicle` form factor, `crates/emu-vmm/src/pipe/car.rs`); the phone pairs
with it over Bluetooth for calls (HFP), contacts (PBAP), messages (MAP) and media (A2DP). Only
`bt-netsim` carries this: there is no port-forward fallback. emu's controller has no SCO
([research/bluetooth.md](../research/bluetooth.md), "emu's controller"), so call audio works only
through rootcanal. Until the bridge exists, the adapter refuses `car-phone` with that reason
instead of making a link that can never be active.

### Headsets (`headset-phone`, #426)

Neither visionOS nor Android XR headsets have a watch-style pairing in the simulators, so
`headset-phone` with a headset is a `network` link with roles: the same mechanism, plus the
headset and phone roles so the desktop can draw them as a pair. Android XR audio and display
glasses do pair: stock pairs a glasses AVD with one phone AVD over Bluetooth, and the app runs on
the phone. A glasses member gets `bt-netsim` under the same kind, and is refused until the
netsimd bridge (#435) exists. Details: [research/headset-pairing.md](../research/headset-pairing.md).

### A shared virtual network (`network`)

What each platform has today:

- iOS simulators are host processes on the host's network stack (`crates/sim-netshape/src/lib.rs`:
  "Simulator apps are host processes"). Simulators on one host already reach each other and the
  host's listeners, and Bonjour runs through the host. iOS members need nothing.
- Each Android guest sits alone behind its own NAT at 10.0.2.15, reaches the host's loopback at
  10.0.2.2, and takes no inbound connection except through adb (`redir` is "not implemented in
  simee yet", `apps/emu/src/console.rs`). Multicast doesn't leave the guest, so mDNS and Nearby
  don't cross.

Mechanisms, in order of work:

1. **`net-forward`** (now): the link names the ports its Android members listen on
   (`ports: {"pixel-9": [8080]}`). The adapter adds a host forward for each (`adb forward`, or
   emu's `redir` once it exists) and saves the host port. Peers connect to 10.0.2.2:<port> from
   Android and 127.0.0.1:<port> from iOS. `link_status` lists each forward and whether it
   listens. No discovery: an app has to be told the address.
2. **`net-segment`** (follow-up): a second virtio-net NIC on each Android member on a link subnet,
   whose frames emu relays to the other members' emu processes over Unix datagram sockets in a
   per-link directory: one L2 segment with multicast, so mDNS and Nearby work between Android
   guests. Stock has the same idea for Wi-Fi (`-android-wifi-server-port` and
   `-android-wifi-client-port` in the stock emulator's help, 37.x). It lives in emu, so a server
   restart doesn't touch it.
3. Discovery between iOS and Android members would need an mDNS reflector between the host and
   the segment. Not designed here (open question 8).

## The adapter interface

Pure logic in `crates/sim-core/src/link.rs`: the record, kinds, roles, kind inference, member
rules, the state transitions, boot order, the `links.json` reader and writer, and `rev`
reconciliation. Unit tested there, no I/O.

The store and adapters in `crates/sim-api/src/links/`: `store.rs` (the files and locks, both
platforms), `coresim.rs` (macOS-gated like `ios.rs`, with an `ios_unavailable.rs`-style stub),
`android.rs` (`adb-forward`, `net-forward`, later `bt-netsim` and `net-segment`), and `fake.rs`.

```rust
pub trait LinkAdapter {
    /// Which mechanism carries `kind` between these members, or why none can. No side effects.
    fn plan(&self, kind: LinkKind, members: &[Member]) -> Result<Mechanism, String>;
    /// Sets the link up for the members that run now; returns params to save and the state.
    fn establish(&mut self, link: &Link) -> Result<(Params, State, String), String>;
    /// Re-derives the state, with what was checked (`link_status`'s `checks`).
    fn probe(&mut self, link: &Link) -> (State, String, Vec<Check>);
    /// Called after a member booted: re-adds forwards, reconnects, reapplies.
    fn member_booted(&mut self, link: &Link, member: &Member) -> Result<(), String>;
    /// Removes what `establish` set up, as far as the running members allow.
    fn teardown(&mut self, link: &Link) -> Result<(), String>;
}
```

`plan` runs before `link` writes anything, so an incompatible pair explains itself first (#428's
"Errors" bullet): no companion app on the image, a runtime CoreSimulator won't pair, `car-phone`
without the Bluetooth bridge.

## API

Four methods in `METHODS` (`crates/sim-api/src/methods.rs`, listed in `ALL_PLATFORMS`), then the CLI subcommands
(`simee link`, `simee unlink`, `simee links`, `simee link-status`) and the MCP tools with no code
in `sim-mcp`, as AGENTS.md says. Every link object has one shape:

```json
{
  "id": "lnk-3f9a2c41",
  "kind": "watch-phone",
  "mechanism": "adb-forward",
  "state": "pending",
  "reason": "wear-5 is shut down",
  "since": "2026-11-28T10:41:07Z",
  "active": true,
  "members": [
    {"device": "pixel-9", "platform": "android", "role": "phone", "running": true},
    {"device": "wear-5", "platform": "android", "role": "watch", "running": false}
  ]
}
```

| Method | Params | Returns | Effect |
| --- | --- | --- | --- |
| `link` | `devices` (2+), `kind?`, `ports?` (network), `active?` (watch-phone), or `link` (an id, to re-pair a broken one) | `{link}` | `Set` |
| `unlink` | `link`, or `device` with `kind?` (every link of that device, or of that kind) | `{unlinked: [ids], teardown: [{id, error?}]}` | `Set`, or `Destroy` if CoreSimulator's unpair erases the watch |
| `links` | `device?`, `kind?`, `state?` | `[link]`, state from files and running members only | `Read` |
| `link_status` | `link` or `device` | `{link, checks: [{name, ok, detail}]}`, probed now | `Read` |

- `link` with members already linked by the same kind returns that link (and re-establishes it if
  broken): repeating it changes nothing more, so `Set` is right. `active: true` on a
  `watch-phone` is `pair_activate`: it makes this one the phone's active watch.
- `devices` and `info` gain `links: [{id, kind, peer, role, state}]` per device, which #423 asks
  for ("devices shows each device's pair").
- #423 and #424 name the calls `pair`, `unpair` and `pairs`. This design has one set of names for
  every kind; the desktop and docs may say "pair" for `watch-phone`. Open question 1.

### A companion on `create` and `boot`

- `create {..., companion: <device>, kind?}` creates the device and links it in one call: "New
  Apple Watch paired to iPhone 17". `plan` runs first, so an incompatible companion fails before
  anything is created. If `link` fails after the create, the new device is deleted, as a cancelled
  `create` already is (`methods.rs`: "a `create` or `clone` cancelled while it ran deletes the
  device it made").
- `boot {device, companions?: "boot" | "leave"}`: `boot` boots the device and, with `boot`, every
  device it is linked to that isn't running, as one admitted group. The default is `leave` (open
  question 2): a boot never starts devices the caller didn't name.
- `boot_many` takes linked devices as they are listed; when a group's members are all in the list,
  it orders and admits them as one unit (below).

### Boot order

1. Within a group, the `phone` boots first and the rest wait for its boot to finish
   (`sys.boot_completed`, or iOS boot status), not for it to settle.
2. `watch`, `car` and `headset` members then boot in parallel.
3. `network` peers have no order: they boot in parallel.
4. After each member boots, `member_booted` runs for each of its links (the hook point is where
   `ios_booted` and the Android camera links are reapplied today).

Phone first because the watch's companion sync, the IDS relay and the forwards on the phone's side
expect the phone up. A group whose phone fails to boot doesn't boot the rest; each gets the
phone's error as its reason.

## Lifecycle and failure modes

| Event | What happens |
| --- | --- |
| A member shuts down (simee, `adb emu kill`, crash) | Links go `pending`, reason names the member. Its peers keep running (open question 3). The next boot of it runs `member_booted`. |
| A member is deleted through simee | `delete` unlinks it first (every link, every member's copy, `teardown` on the rest) and returns `unlinked: [ids]`. No broken link is left behind. |
| A member is deleted outside simee (`simctl delete`, `emu -delete-avd`) | Its peers' copies name a device that is gone: `broken`, "wear-5 no longer exists". `unlink` clears it. CoreSimulator drops its own pair with the device. |
| A member is erased | iOS: links kept (the file is saved around `erase`); the guest's own pairing state is gone, so the link goes `pending` and `member_booted` re-establishes it. (`erase` is iOS-only today; an Android erase, when it comes, keeps `simee/links.json` the same way.) |
| A member is cloned | The clone isn't a member; its copied `links.json` (iOS) is ignored and removed. |
| The simee server restarts | Nothing to restore: the records are files, the live parts are in CoreSimulator, adb, emu and netsimd. The first `links` re-derives every state. |
| The host reboots | iOS pairs are in `device_set.plist`; adb forwards are gone until a member boots and `member_booted` re-adds them. |
| A write to one member's copy fails | The others hold the higher `rev`; the next read writes it back. The call reports the copy it couldn't write. |
| A member changes runtime or system image | `plan` fails on the next `link_status`: `broken`, with `plan`'s reason. |
| netsimd stops | `bt-netsim` links go `broken`, "netsimd is not running"; the next `member_booted` or `link` restarts it. |

### Snapshots, export and import (#169, #170)

- Snapshots of a running Android device don't exist yet (blocked on the renderer, #169), and iOS
  has none, so "a snapshot of a pair" today means its disks: an export, or a clone.
- `export` (Android only today, `ANDROID_ONLY` in `methods.rs`) packs only `config.ini`,
  `boot.prop` and the cloned disks (`allowed_path` in `crates/emu-avd/src/bundle.rs`), so
  `links.json` doesn't travel, and it shouldn't by itself: its peer may not exist on the receiver.
  The manifest gains `links: [{kind, role, peer: {platform, form factor, system image or runtime
  identity}, params}]`, informational, so an import can say "this was paired with a Pixel 9 on
  API 36" and the receiver can link it again.
- `import` makes no links. It returns the bundle's `links` as `relink_hints`.
- Exporting a whole group (every member in one bundle, the link recreated on import between the
  new devices, the saved Bluetooth addresses kept so bonds hold) is a follow-up for #169
  (open question 10).

### Farm admission

A linked group is one unit:

- `boot` with `companions: "boot"`, `create` with a companion, and `boot_many` over a whole group
  pass every member that isn't running to `admitted` as one list of `Item`s, with the group's
  size as its concurrency (`judge_now` already takes a list), so the cost judged is the group's
  sum.
- `admission: "refuse"` refuses the whole group or none of it. `admission: "wait"` waits until the
  whole group is admitted, then starts it in boot order; it never starts the phone and leaves the
  watch waiting on the host.
- In `boot_many`, a group takes one pacing slot and its members' costs together.
- `devices_usage` gains each device's link ids, so a farm can sum a group's cost.

## MCP

- The four tools go in the `lifecycle` toolset (`Group::Lifecycle` in
  `crates/sim-api/src/catalog.rs`; pairing is creating and booting devices together), with
  effects as in the API table. `links` and `link_status` also go in `core`, since an agent driving
  a watch needs to see its phone without asking for a toolset. No new `Group`.
- New parameter names get their types in `crates/sim-api/src/schema.rs` (`devices`, `kind`,
  `link`, `companion`, `companions`, `ports`, `active`; a test names the missing ones).
- `link` goes in `tools::LONG_RUNNING`: a Wear OS or Bluetooth pairing can take many seconds.
  It reports its steps (planned, establishing, waiting for the companion) through the progress
  hook as `boot` does.
- None of them goes in `IN_CALLER` (`crates/sim-api/src/forward.rs`): no link state lives in the
  calling process, so a matching server gives the same result.
- Resources in `crates/sim-mcp/src/resources.rs`: `simee://links` (read through `links`) and
  `simee://devices/{device}/links`, both subscribable through the existing watcher, so the desktop
  and an agent see a link go `active` or `broken` without polling the tool.
- `docs/mcp.md`'s tool table and toolset rows change in the same PR (a test fails otherwise).

## Testing

Per the owner's rule of 2026-11-28, lanes cover behaviour with fakes and the harness, merge on
green, and list the device checks that run on main afterwards.

With fakes, in each PR:

- `sim-core`: kind inference and member rules for every pair of form factors; every state
  transition; boot order for each kind; `links.json` round trip, a bad record skipped with a warning,
  a copy that doesn't name its own device ignored; `rev` reconciliation of two disagreeing copies.
- `sim-api` with `links/fake.rs`, a scripted adapter (plan refuses, establish fails, probe flips
  state): `link`, `unlink`, `links`, `link_status`; `create` with a companion rolled back when the
  link fails; `delete` unlinking first; the file saved around iOS `erase`; two concurrent `link`
  calls on one phone writing both copies; `companions: "boot"` booting in order against fake
  devices; admission refusing and waiting for a group as one unit (`Session.fake_host`).
- Android against `crates/sim-android/src/testing.rs` (the fake emulator and fake `adb`): the
  forwards the `adb-forward` and `net-forward` adapters ask for, and their re-adding after a boot.
- iOS against a fake behind a trait, as `camera.rs` does with `IosHost`: the CoreSimulator calls
  the adapter makes, and a pair that already exists in the fake's set adopted as the link.
- MCP: the tool list and toolsets, the resources and their subscriptions, `docs/mcp.md` in step.

Device checks after merge, each with the result expected:

- iOS: `simee create W "Apple Watch Series 10 (46mm)" "watchOS 26.0" --form-factor watch
  --companion <iPhone UDID>`, then `simee boot W --companions boot`: the phone boots first,
  `simee links` shows the link `active`, `xcrun simctl list pairs` shows the same pair as
  (active, connected). A WatchConnectivity message crosses in the #423 fixture. `simee serve`
  restarted: the link is still `active`. `simee unlink`: note whether the watch was erased.
- Wear OS: a phone AVD with Google Play and a Wear OS AVD linked with `adb-forward`: `adb -s
  <phone> forward --list` and `adb -s <watch> reverse --list` show the pair's ports, the phone's
  companion app shows the watch connected, and a Data Layer message reaches the watch (#424,
  #446). Restart the adb server: the next `link_status` re-adds both and the link returns to
  `active`.
- Network: two Android AVDs and one iPhone simulator in one `network` link with a listener on one
  AVD: the other AVD connects to 10.0.2.2:<port> and the iPhone to 127.0.0.1:<port>.
- Farm: `boot_many` over two watch pairs with `admission: "wait"`: each group starts whole.
- Each run in the AutoMobile harness on every device type touched, as for any change.

## Open questions and owner decisions

Each has the default this design uses if nobody decides otherwise.

1. **One API or `pair` names too?** #423/#424 say `pair`, `unpair`, `pairs`, `pair_activate`.
   Default: only `link`, `unlink`, `links`, `link_status`; `active: true` on `link` is
   `pair_activate`; UI and docs say "pair" for `watch-phone`.
2. **Does booting a member boot its peers?** Default: no (`companions: "leave"`); the caller asks
   with `companions: "boot"`. The desktop's "boot group" sends that.
3. **Does shutting down the phone shut down its watch?** Default: no; the watch's link goes
   `pending`. `shutdown {companions: "shutdown"}` is added only if asked for.
4. **Deleting a member.** Default: `delete` unlinks it everywhere and reports it, rather than
   refusing or leaving a broken link.
5. **DHU and CarPlay: links or projections?** Default: projections on the phone, outside the link
   store; `car-phone` is AAOS + phone only. If the DHU should show as a device, it becomes a `car`
   member without an AVD.
6. **`headset-phone` mechanism.** Default: a `network` link with roles for headsets; `bt-netsim`
   for Android XR glasses, which #426 found do pair with a phone
   ([research/headset-pairing.md](../research/headset-pairing.md)).
7. **Cross-platform links.** Default: only `network` crosses platforms.
8. **Network mechanism and discovery.** Default: `net-forward` now, `net-segment` (emu-to-emu L2,
   mDNS between Android guests) as a follow-up issue; no iOS-Android mDNS reflector until a user
   needs it.
9. **netsimd's lifetime and cost.** Default: started when the first `bt-netsim` link needs it and
   stopped when the last one ends, never for unlinked devices; its idle CPU and memory go under
   "Not yet measured" in `docs/benchmarks.md` until measured, and it isn't a farm default before
   then.
10. **Export of a linked device.** Default: a single-device export drops the link and records it
    in the manifest as hints; a group export is a follow-up under #169.
11. **Several watches on one phone.** Default: allowed, one active per phone on iOS as CoreSimulator
    has it; on Wear OS every linked watch may be active.
12. **`unlink` erasing the watch.** If CoreSimulator's unpair erases it (to verify), default:
    `unlink` says so in its description and result and is classed `Destroy`, with no extra
    confirmation parameter.
13. **Group size.** Default: `network` links of at most 16 members; pairs are exactly two.
14. **Stable Bluetooth addresses.** Default: emu takes a saved address (`-bt-addr`) for linked
    devices only; unlinked devices keep `address_for`.

## Order of work

1. This doc (#427), reviewed.
2. The core: `sim-core` model and `sim-api` store, the four methods, `create`/`boot` companions,
   admission as a group, MCP tools and resources, all against `links/fake.rs` (#427).
3. iOS `coresim-pair` (#423), then Wear OS `adb-forward` (#424), then `net-forward`.
4. The desktop on the link API (#428).
5. emu's netsimd bridge and `bt-netsim` (Wear OS over Bluetooth, AAOS + phone, #424/#425), and
   `net-segment`, as follow-up issues.
