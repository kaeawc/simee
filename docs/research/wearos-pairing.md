# Pairing a Wear OS emulator with a phone

How the stock tools pair a Wear OS emulator with a phone, what emu has to provide, and a plan for
`pair`, `unpair` and `pairs` on Android. Issue [#424](https://github.com/kaeawc/simee/issues/424),
part of the device pairing epic [#429](https://github.com/kaeawc/simee/issues/429).

Evidence: Android Studio's pairing assistant source (the `wear-pairing` module of the
[JetBrains/android mirror](https://github.com/JetBrains/android/tree/fb4934312e94/wear-pairing/src/com/android/tools/idea/wearpairing),
read at `fb4934312e94`, 2026-11-27), AOSP's adb sources, and developer.android.com, all read on
2026-11-28. **Nothing here was run on a device**: the research host has no KVM, and no image or
SDK component was downloaded. What the Studio source shows is stated as fact; what follows from
it, or from the closed-source companion app and Google Play services, is marked *inferred* or
*unverified*, and the device checks that settle each are listed in [Open questions](#open-questions).

## Summary

- Stock pairs an emulated watch with a phone over **TCP through adb, not Bluetooth**. Studio's
  assistant runs `adb -s <phone> forward tcp:<port> tcp:5601` and `adb -s <watch> reverse tcp:5601
  tcp:<port>` on a free host port (from 5602), so the watch's Google Play services, which dials
  `localhost:5601`, reaches the companion app listening on the phone's `localhost:5601`.
- The phone needs **Google Play** (an API 30+ image with the Play Store) and the Wear OS companion
  app, which the user installs from Play once. The watch needs API 28+.
- Everything after that is `adb`: a broadcast to the watch, an activity launched on the phone and a
  logcat line that reports progress. The one step the source shows as needing a person is a
  **consent** screen in the companion app.
- emu needs **nothing new for the link itself**: `adb forward` and `adb reverse` live in the adb
  server and `adbd`, and emu's guests already speak adb over the vsock bridge. The work is in
  simee: an adb bridge, a preflight, the companion handshake, and a saved link that is rebuilt
  whenever a device or the adb server restarts.
- Recommendation: **option 1, port forwarding driven by simee** (below). A virtual Bluetooth link
  between emu instances is a large piece of work that the stock Wear OS emulator doesn't use for
  pairing; a shared virtual network belongs to the link model
  ([#427](https://github.com/kaeawc/simee/issues/427)), not to this pairing.

## How stock pairs

### The pieces

| Piece | Role | Source |
| --- | --- | --- |
| Pairing assistant | Android Studio's Device Manager → *Pair Wearable*: starts both AVDs, bridges them, drives the companion, saves the pair and re-bridges it when the devices come back | [connect-phone](https://developer.android.com/training/wearables/get-started/connect-phone), `WearPairingManager.kt` |
| Companion app on the phone | `com.google.android.apps.wear.companion` (the Pixel Watch app) or the older `com.google.android.wearable.app`; the watch names its own through `settings get secure cw.oem_companion_package` or `ro.oem.companion_package`, falling back to `com.google.android.wearable.app` | `PairingFeature.kt` |
| Google Play services (GMS) on both | Hosts the Wearable Data Layer node; takes the `com.google.android.gms.wearable.EMULATOR` broadcasts the assistant uses to refresh the connection and read the pairing status | `DeviceConnection.kt` |
| adb | The only transport between the two emulators: a forward on the phone, a reverse on the watch | `WearPairingManager.kt` |

### What the assistant does, in order

From `DevicesConnectionStep.kt`, `WearPairingManager.createPairedDeviceBridge` and
`NonInteractivePairing.kt`:

1. Launch both AVDs if needed and wait for `dev.bootcomplete` on each.
2. Wait up to 50 s for each device's pairing status (`am broadcast -a
   com.google.android.gms.wearable.EMULATOR --es operation get-pairing-status`), which prints the
   device's own Data Layer node (`Local:[<id>]`) and one line per peer (`Peer:[<id>,<connected>,<enabled>]`).
   Before the status operation existed, it read `dumpsys activity service WearableService`.
3. Check Google Play services is new enough on each side (version codes: reverse forwarding
   210915000 on the watch, multi-watch 213204000 on the phone, status 214412000, refresh
   213013000). An older one opens the Play Store page for GMS on that device and stops.
4. If the watch already has a peer that isn't this phone, it needs a **factory reset** (a Wear OS
   emulator pairs with one phone; a phone can pair several watches).
5. Check the companion is installed on the phone; if not, open its Play Store page and wait for
   the user to install it. A phone's ABI matters: the older companion has no x86_64 build.
6. Bridge:
   ```
   adb -s <phone> forward tcp:<hostPort> tcp:5601     # hostPort: first free from 5602
   adb -s <watch> reverse tcp:5601 tcp:<hostPort>
   adb -s <watch> shell am broadcast -a com.google.android.gms.wearable.EMULATOR \
       --es operation refresh-emulator-connection     # older GMS: force-stop GMS instead
   ```
7. Start the pairing on the phone: read `logcat`, then `am start -n <companion>/.EmulatorActivity
   --es emulator-name "<watch AVD name>" --es emulator-id "<watch node id>"`. The companion logs
   `[EMULATOR_PAIRING:<state>]` with `STARTED`, `CONSENT`, `PAIRING`, `SUCCESS`, `FAILURE` or
   `CANCELLED`. On `CONSENT` Studio tells the user to finish the steps in the companion app on the
   phone. The assistant waits one minute, then checks the status; if the activity doesn't exist
   (an old companion), it launches the companion's launcher activity with `monkey` and waits for the
   user to pair by hand.
8. Connected means `get-pairing-status` on the phone lists the watch's node with `connected` and
   `enabled` both true.

### The network path

- The watch's GMS connects out to `localhost:5601`. `adb reverse` makes `adbd` on the watch listen
  there and open a stream to the host's `<hostPort>` through the adb transport (the `reverse:`
  service in AOSP's `daemon/services.cpp`); the adb server on the host connects to that port; the
  phone's forward carries it to the phone's `adbd`, which connects to `localhost:5601` inside the
  phone, where the companion app listens. The Data Layer's own traffic then runs over that one
  stream. The older manual recipe for a physical phone was one forward of the host's port 5601 to the
  phone (`adb -d forward tcp:5601 tcp:5601`), with the watch emulator dialling the host's 5601;
  the assistant's reverse replaced the host hop on the watch's side. (The recipe is from
  third-party write-ups and the Android docs' older text; not checked against a running emulator.)
- **There is no Bluetooth link.** Nothing in the assistant touches a Bluetooth controller. The
  Data Layer overview says it uses Bluetooth when connected and the cloud otherwise; for an
  emulator pair the transport is the forwarded socket (*inferred* from the source, since Play
  services is closed). The phone's Bluetooth adapter does still matter to the companion: current
  Pixel Watch app builds crash on a phone emulator until *Nearby devices* is allowed
  ([emulator known issues](https://developer.android.com/training/wearables/get-started/emulator)).
- The pair outlives the bridge's processes but not the bridge. adb's forward lives in the adb
  server and the reverse in the watch's `adbd`, so both are gone after an adb server restart or a
  reboot of either device. GMS keeps the pairing itself (node ids, the cloud network id) in each
  device's `/data`, so rebuilding the bridge and sending the refresh broadcast is all Studio does
  when a saved pair's devices come back (`updateForwardState`: both online after offline →
  `createPairedDeviceBridge`, no companion step). Studio saves the pairs in its own settings.
- **Unpair** removes both forwards on the phone (5601 and `<hostPort>`), the reverse on the watch,
  runs `pm clear com.google.android.apps.wear.companion` on the phone (the Pixel companion would
  reconnect through its cloud sync otherwise), and refreshes the watch's connection.

### Which images pair

| | Requirement | Source |
| --- | --- | --- |
| Phone | API 30+ **and** the Play Store (a `google_apis_playstore` image); a physical phone works too | `DeviceListStep.kt`; the docs' "Note" in connect-phone |
| Watch | API 28+; a Wear OS image (they ship Google Play services, *inferred*: the assistant reads its pairing status) | `DeviceListStep.kt` |
| Companion | installed on the phone from Play, so a Google account on the phone | connect-phone |
| Google Play services | new enough on both, see step 3 | `PairingFeature.kt` |

So **yes: the phone image needs Google Play**, and a signed-in Google account to install the
companion. A plain `google_apis` phone, which is simee's default image, can't be paired by the
assistant. emu already provisions Play images (`emu -provision 36 -tag google_apis_playstore`,
[android.md](../android.md)).

### Can it be done host-side without UI steps?

Mostly. Studio drives the pairing through adb alone, and its source has a `NonInteractivePairing`
class for the companion handshake, so simee can do the same without Studio. What stays manual:

- **One time per phone image:** sign in to a Google account and install the companion from Play.
  Not scriptable without handling credentials, and simee won't. Keep a phone AVD that has done this
  and pair watches to it.
- **Consent:** the `CONSENT` state means the companion shows a screen the user finishes. Whether
  simee can finish it with UI automation (`tap` on the screen found by `ui_dump`) is *unverified*:
  it depends on the companion's screens, which Google changes. Treat it as a manual step first,
  with the pair left `pending` and the reason reported, then try automation on a device.
- *Unverified:* whether the Google account has to be copied to the watch during pairing (the
  manual flow's account step) or the emulator flow skips it.

## What emu needs

Facts about emu: its guests' `adbd` is reached over virtio-vsock through a bridge on the console
port + 1 and announced to the adb server, so `adb shell`, `push` and `install` work
([android.md](../android.md)); its NAT reaches the Internet and the host's loopback at 10.0.2.2
(Play Store and the companion's sign-in need it); its Bluetooth controller has no peers
([bluetooth.md](bluetooth.md)); it has no inbound port forwarding (`redir`) and its NAT is
per process.

### Option 1: port forwarding through simee (adb forward and reverse)

simee runs the same adb commands Studio does, from `sim-android` (which already runs the host's
adb per device with deadlines, `apps.rs`).

- emu changes: **none expected.** `forward` is entirely host-side; `reverse` needs `adbd` and the
  adb server, which stock emulators and emu share. *Unverified on emu*: that `reverse` streams
  survive emu's vsock bridge under load, and that two emu instances' serials (`emulator-5554`,
  `emulator-5556`) give the adb server no trouble.
- Works for a stock AVD as well, since nothing is emu-specific; the same code pairs an AVD the
  stock emulator runs (as `install` and `launch` already do).
- Cost: no process of its own and no per-device CPU beyond the forwarded stream.
- Limits: the bridge must be rebuilt after an adb server restart or a device reboot (a
  reconciler, below), and ports from 5602 up are claimed on the host.

### Option 2: a virtual Bluetooth link between two emu instances

What it would take: bridge the two controllers' `bluetooth` ports (H4) so one's inquiry finds the
other, either through netsimd's raw HCI socket (option 2 of [bluetooth.md](bluetooth.md): not
verified yet there) or a controller model in emu's own `bt/` (option 3 there, "don't build"). Then
the phone's companion would have to treat the emulated watch as a Bluetooth peer.

- It would match what a real watch does, and would let stock emulators and emu devices pair
  with each other and with Bluetooth audio tests.
- It is **not what stock does for Wear OS pairing**: the assistant never uses Bluetooth, and
  nothing found says the Wear OS emulator image (GMS in emulator mode) accepts a Bluetooth pairing
  with a companion. *Unverified*, and a large cost to find out: a host-side air model, LE and
  BR/EDR links, GATT and bonding, and per-device CPU that isn't measured.
- It doesn't remove option 1's manual steps (Play, companion, consent).

### Option 3: a shared virtual network between the two emu instances

Give both guests one L2 or L3 segment, so the watch reaches the phone by address. Needs a link
between two emu processes' NATs (a host-side switch or a socket pair), and a way for the watch's
GMS to dial the phone instead of `localhost:5601` (which would still need the reverse, or a
rewrite in the guest). Alone it doesn't pair anything.

It is still worth building for the `network` link kind in
[#427](https://github.com/kaeawc/simee/issues/427) (two devices that talk to each other: Nearby,
local servers, device-to-device apps). Track it there.

### Recommendation

**Option 1.** It is what stock does, needs no emu change, works on both emulators, and has the
fewest unknowns. Do not build a Bluetooth link for this pairing. Revisit it only if a device check
shows the companion needs one, or when Bluetooth peers are wanted for their own sake
([bluetooth.md](bluetooth.md)'s option 2).

## Plan for `pair`, `unpair` and `pairs` on Android

The names and shapes follow the iOS pairing issue
([#423](https://github.com/kaeawc/simee/issues/423): `pair {watch, phone}`, `unpair {pair|watch}`,
`pairs`, `pair_activate`) and the link model ([#427](https://github.com/kaeawc/simee/issues/427):
a link has a `kind`, devices, a state of pending, active or broken, is saved with each device, and
boot order is automatic, phone first). Neither has landed, so reconcile the result shapes when
they do; the Android adapter below is the part this issue owns.

### API

- `pair {watch, phone}`: both name a device (AVD name or serial, as every method takes). Returns
  the pair: `id`, `kind: "watch-phone"`, `platform: "android"`, the two devices in the existing
  device shape, `state` (`pending`, `active`, `broken`) and, when not `active`, a `reason`
  and `needs` (`"consent on <phone>"`, `"companion app on <phone>"`).
  Fails early, naming the fix, when: a device isn't the right form factor (`Kind`), the phone
  isn't API 30+ with Play, the watch is API < 28, the companion isn't installed, Play services is
  too old, or the watch is paired to another phone (it names the phone and says a factory reset is
  needed; a `force` flag is a later decision).
- `unpair {pair|watch}`: removes the bridge on each running device, clears the companion's data on
  the phone, refreshes the watch, and deletes the record on both. A stopped device keeps a
  tombstone in its `simee/` directory, applied at its next boot, so GMS doesn't stay paired to a
  link simee has forgotten.
- `pairs`: every pair with live state: `active` when the phone's `get-pairing-status` lists the
  watch with `connected` and `enabled`; `pending` while consent is open; `broken` with a reason
  (`phone not running`, `bridge down`, `disconnected`) otherwise. `devices` lists each device's
  pair, as #423 plans for iOS.
- `pair_activate`: not offered on Android at first. On iOS it picks the one active watch of a
  phone; here Play services supports several watches on one phone itself. Say so in `help`
  (platforms) and return `Error::Unavailable` with that reason.
- CLI and MCP: a CLI subcommand per method and nothing in `sim-mcp` (tool per method, parameter
  types in `schema.rs`, `pair` in `tools::LONG_RUNNING` since consent can take minutes, and
  `docs/mcp.md`'s table).
- Safety: strings that reach a guest shell are validated first, as Studio validates them: package
  names against Android's grammar, node ids against `^[a-zA-Z0-9_.-]{1,64}$`, the watch's AVD
  name quoted.

### Persistence and boot

- One record per pair in `<avd>/simee/links.json` of **both** AVDs ("saved with each device",
  #427): pair id, role, the peer's AVD name, host port, the watch's node id, the companion's
  package. State is derived, never stored.
- A reconciler (in the process that owns the devices: the API server) rebuilds the bridge when
  both devices of a saved pair are running and it is down: at start, on a device's boot, and
  when the adb server restarts (poll `adb forward --list` and `reverse --list`). It picks a free
  port again if the saved one is taken, runs the three bridge commands from step 6 and the status
  check. No companion step: GMS kept the pairing.
- Boot order: the phone first (#427); booting a watch with a saved pair boots its phone, if the
  API method that boots takes the companion.
- Shutdown or deletion of one device: the other's pair is `broken` (`peer not running`), not
  removed. Deleting a device runs `unpair` first.

### Testing

Pure logic in `sim-core` with unit tests: the parsers for `get-pairing-status` and the
`[EMULATOR_PAIRING:*]` lines, the companion choice, the GMS version gates, the preflight result
and the pair state. The bridge and handshake in `sim-android` against a fake adb (as
`install` and `launch` are tested). The persistence and reconciler in `sim-api` with fake
adapters. Real devices after merge, per the post-merge device rule: the checks in
[Open questions](#open-questions) and #424's "done when" (a Data Layer message reaching a fixture
app on the watch).

## Open questions

Each is a device check for after the implementation lands, on a Mac with the Play images:

1. Do `adb forward` and `adb reverse` work over emu's vsock bridge, for a stream that stays open
   for hours, with two emu devices and a stock one on one adb server?
2. Does the emulator-mode flow need a Bluetooth adapter that is *on*? emu's controller is on but
   has no peers; stock's has netsim.
3. Which companion does each current Wear OS image ask for, and does its `EmulatorActivity` exist
   on the Play image's version?
4. What exactly does `CONSENT` show, and can `tap` finish it? Does pairing copy the account to the
   watch?
5. Does the pair survive a reboot of each device with only the bridge rebuilt (as Studio's
   behaviour implies), and a restart of the adb server?
6. Does a Data Layer message cross, and through the forwarded socket rather than the cloud?
   Check with `dumpsys activity service WearableService` on both while the phone has no network.
7. How long do pairing, and the bridge's rebuild after a boot, take? Put the numbers in
   [benchmarks.md](../benchmarks.md) if they are user-visible.

## Follow-up issues

Filed from this research, in build order:

1. [#442](https://github.com/kaeawc/simee/issues/442): device check that `adb forward` and
   `reverse` hold between two emu devices (the go or no-go for option 1).
2. [#443](https://github.com/kaeawc/simee/issues/443): sim-core logic: status and log parsers,
   companion choice, Play services gates, preflight, pair state.
3. [#444](https://github.com/kaeawc/simee/issues/444): the adb bridge and companion handshake in
   `sim-android`, against a fake adb.
4. [#445](https://github.com/kaeawc/simee/issues/445): `pair`, `unpair` and `pairs` on Android,
   saved on both devices and rebuilt on boot (after #427's core store); CLI and MCP.
5. [#446](https://github.com/kaeawc/simee/issues/446): device verification of the open questions,
   a Data Layer fixture app and the e2e test.

Not filed: a Bluetooth link between emu instances (not what stock does for Wear OS pairing; see
[bluetooth.md](bluetooth.md) if a device check shows it is needed) and a shared virtual network
(#427's `network` kind).

## Sources

- Android Developers: [Connect a watch to a phone](https://developer.android.com/training/wearables/get-started/connect-phone),
  [The Wear OS emulator](https://developer.android.com/training/wearables/get-started/emulator),
  [Debug Wear OS apps](https://developer.android.com/training/wearables/get-started/debugging),
  [Data Layer overview](https://developer.android.com/training/wearables/data/overview),
  [Android Debug Bridge](https://developer.android.com/tools/adb) (`forward`, `reverse`).
- Android Studio's Wear OS pairing assistant (Apache-2.0), `wear-pairing` module, at
  `fb4934312e94`: [`WearPairingManager.kt`](https://github.com/JetBrains/android/blob/fb4934312e94/wear-pairing/src/com/android/tools/idea/wearpairing/WearPairingManager.kt)
  (the bridge, unpair, re-bridging), [`DeviceConnection.kt`](https://github.com/JetBrains/android/blob/fb4934312e94/wear-pairing/src/com/android/tools/idea/wearpairing/DeviceConnection.kt)
  (the `EMULATOR` broadcasts, status parsing), [`NonInteractivePairing.kt`](https://github.com/JetBrains/android/blob/fb4934312e94/wear-pairing/src/com/android/tools/idea/wearpairing/NonInteractivePairing.kt)
  (`EmulatorActivity`, `[EMULATOR_PAIRING:*]`), [`PairingFeature.kt`](https://github.com/JetBrains/android/blob/fb4934312e94/wear-pairing/src/com/android/tools/idea/wearpairing/PairingFeature.kt)
  (companions, version gates), [`DevicesConnectionStep.kt`](https://github.com/JetBrains/android/blob/fb4934312e94/wear-pairing/src/com/android/tools/idea/wearpairing/DevicesConnectionStep.kt)
  (the order of steps), [`DeviceListStep.kt`](https://github.com/JetBrains/android/blob/fb4934312e94/wear-pairing/src/com/android/tools/idea/wearpairing/DeviceListStep.kt)
  (API and Play requirements).
- AOSP adb: [`daemon/services.cpp`](https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/daemon/services.cpp)
  (the `reverse:` service), [`adb_listeners.cpp`](https://android.googlesource.com/platform/packages/modules/adb/+/refs/heads/main/adb_listeners.cpp)
  (listeners for forward and reverse).
- In this repository: [bluetooth.md](bluetooth.md) (emu's controller, netsim),
  [android.md](../android.md) (adb bridge, NAT, Play images), [form-factors.md](../form-factors.md),
  issues [#65](https://github.com/kaeawc/simee/issues/65) (HCI controller, closed),
  [#423](https://github.com/kaeawc/simee/issues/423), [#427](https://github.com/kaeawc/simee/issues/427).
