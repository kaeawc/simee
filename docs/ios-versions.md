# Supported iOS and tvOS versions

What works on each installed simulator runtime, run feature by feature against a fresh device of
each. Measured 2026-11-26 on an Apple silicon Mac (M3 Max, macOS 26.6.2) with Xcode 26.6
(17F113, CoreSimulator 1174.21). The host was heavily loaded by other work that day, so no timings
are given here; see [benchmarks.md](benchmarks.md) for those.

How: `simee create` an iPhone 16 (iOS 18), iPhone 17 (iOS 26 and 27) or iPhone Duo (the only
device type of the iOS 27.1 and `iOS-27-2-iPhone19-4` runtimes), `simee boot`, wait a minute, then
`scripts/ios-matrix.sh <udid>`, `scripts/parity.sh <udid>` and
`SIMEE_E2E_UDID=<udid> cargo test -p simee --test e2e`; frame callbacks with
`cargo run -p simee-coresim --example frame_watch -- <udid>`. The AutoMobile harness
(`scripts/automobile-harness.sh`) ran on the oldest and newest runtimes.

## iPhone

| Feature | iOS 18.0 | iOS 18.6 | iOS 26.5 | iOS 27.0 | iOS 27.1 (Duo) | iOS 27.2 | iOS 27.2 (Duo) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `parity.sh` (identity, screenshot size and pixels, apps) | pass | pass | pass | pass | pass | pass | pass |
| e2e suite | pass | pass | pass | pass | pass | pass | pass |
| screenshot | pass | pass | pass | pass | pass | pass | pass |
| tap, swipe, button, key, type | pass | pass | pass | pass | pass | pass | pass |
| rotate (orientation and 90°) | pass | pass | pass | pass | pass | pass | pass |
| frame callbacks (`Display::watch_frames`) | pass | pass | pass | pass | pass | pass | pass |
| install, launch, terminate, uninstall, apps | pass | pass | pass | pass | pass | pass | pass |
| privacy grant/revoke | pass | pass | pass | pass | pass | pass | pass |
| location, appearance, open URL, memory warning | pass | pass | pass | pass | pass | pass | pass |
| status bar, content size, contrast, biometric, keychain | pass | pass | pass | pass | pass | pass | pass |
| push | pass | pass | pass | pass¹ | pass¹ | pass¹ | pass¹ |
| exec, processes, logs, connections, containers | pass | pass | pass | pass | pass | pass | pass |
| network shaping: proxy | pass | pass | pass | pass | pass | pass | pass |
| network shaping: injected (`inject: true`) | pass | pass | pass | pass | pass | pass | pass |
| audio: state, volume, mute, capture start/stop | pass | pass | pass | pass | pass | pass | pass |
| CarPlay attach/detach | pass² | pass² | pass² | unsupported³ | unsupported³ | unsupported³ | unsupported³ |
| AutoMobile harness (getApple, observe, pressButton) | pass | — | — | — | — | pass | pass (folded and unfolded) |
| `fold`, the inner screen (`display=inner`) | — | — | — | — | not run | — (one screen) | pass |

1. In the first minute after boot `push` fails with "Simulator device failed to complete the
   requested operation" while the device's `apsd` is still starting; the same call passes once it
   has. Wait for the device to settle (`simee settle`).
2. The guest connects the car display (SpringBoard logs `Car[2-3]` at 800×480) and taps and
   screenshots reach it, but on this host it stays black on every runtime. Not yet compared with
   Simulator.app's CarPlay window on the same host.
3. `carplay` refuses iOS 27 runtimes: Apple renders CarPlay black there ([CarPlay, per Xcode
   release](#carplay-per-xcode-release)).

## CarPlay, per Xcode release

On Xcode 27 CarPlay moved to Device Hub, whose documentation asks for an iPhone connected over USB
and lists "iOS Simulator is not supported for CarPlay" as a known issue; a feedback report
(FB24785359, open when read on 2026-11-28) says Device Hub's CarPlay plugin asks for an
internal-only entitlement. `sim_core::carplay::runtime_supports` therefore refuses iOS 27 runtimes,
and its message points at an iOS 26 runtime and at kaeawc/simee#441, which stays open (label
`upstream`) until Apple restores simulated CarPlay or documents it as removed. simee has no
real-iPhone path: a cable and a physical device are another product.

**Run the check on each new Xcode release** (and when a new iOS runtime installs), on a quiet host:

```bash
scripts/bench/carplay-check.sh                 # every installed iOS 26+ runtime, in a device set of its own
```

For each runtime it creates an iPhone (the first iPhone type for a runtime that has no `iPhone 17`,
such as the Duo-only ones) with `simee --set`, boots it, calls `carplay on`, takes
`screenshot --display carplay` and calls it rendered when over 1% of the pixels are lit; it prints
the rows below and deletes everything it created. It sets `SIMEE_CARPLAY_UNGATED=1` for the
`carplay` call so iOS 27 runtimes are tried despite the gate, which is the thing under test. Paste
the rows into the table and record the Xcode build. When an iOS 27 runtime renders, drop the gate
in `runtime_supports` and update its tests (`carplay.rs`), `docs/form-factors.md` and the table
above. Pass `CARPLAY_BOOT_WRAP="<boot gate> --"` on a shared host.

| Date | Xcode | Runtime | `carplay on` | Renders |
| --- | --- | --- | --- | --- |
| 2026-11-28 | 26.6 (17F113) | iOS 26.5 (23F77), iPhone 17 | ok | no (screenshot all black) |
| 2026-11-28 | 26.6 (17F113) | iOS 27.0 (24A434), iPhone 17 | refused: `Unsupported Mode: 800x480@60hz` | no (no external display) |
| 2026-11-28 | 26.6 (17F113) | iOS 27.1 (24A94401), iPhone Duo | refused: `Unsupported Mode: 800x480@60hz` | no (no external display) |
| 2026-11-28 | 26.6 (17F113) | iOS 27.2 (24B8117), iPhone 17 | refused: `Unsupported Mode: 800x480@60hz` | no (no external display) |
| 2026-11-28 | 26.6 (17F113) | iOS 27.2 (24B91016s), iPhone Duo | refused: `Unsupported Mode: 800x480@60hz` | no (no external display) |
| not run | 27 | | | Xcode 27 is not installed on this host (the iOS 27 runtimes above are installed under Xcode 26.6) |

Reading it: on iOS 27 the guest itself rejects the CarPlay screen mode, so the gate isn't hiding a
working path. On iOS 26.5 the attach works and touches reach the car display, but its framebuffer
stays black on this host, as footnote 2 says; it has not been compared with Simulator.app's CarPlay
window, so the message's "use an iOS 26 runtime" is the better of the two, not a promise of a
picture. The screenshots and logs: `bench/results/carplay-<date>/`.

## iPhone Duo

The iPhone Duo (`iPhone19,4`) is iOS's foldable: simee lists it as `form_factor: foldable`,
sub-platform `ios` ([form-factors.md](form-factors.md)). It has two built-in screens: the outer one (`LCD`, 1398×2034, the
device type's main screen) and the inner one (`LCD-1`, 2007×2853, a landscape panel its
framebuffer holds a quarter turn from upright). SpringBoard shows the device on one at a time and
leaves the other black. Neither Xcode 26.6's Simulator.app nor Xcode 27.2 (whose DeviceHub replaces
it) has a fold control, and the simulator has no hinge sensor: the guest boots closed, on the outer
screen.

- **Fold and unfold.** SpringBoard's display tool service (`SBSDisplayToolService`, which its
  internal `suiatool display swap` drives) sweeps a synthetic hinge to a position and moves the
  device to the other screen when the sweep crosses its swap boundary (85°, opening from closed).
  `fold` runs a small in-device program (`crates/sim-api/native/displaytool.m`, built on first use
  with the simulator SDK and cached like the network shaper) that asks it to; SpringBoard serves
  clients with the `com.apple.springboard.sbdisplay.service` entitlement, which the program carries
  in `__TEXT,__entitlements`, where the simulator reads a program's entitlements. `simee fold <duo>
  unfold|fold|half-opened|<angle>`; the answer names the screen in use. The hinge keeps its
  hysteresis: opening to the book posture (100°) from closed moves to the inner screen, closing to
  it from open stays there.
- **Which screen.** `screen`, `screenshot`, `tap`, `swipe` and `touch` take
  `display=main|outer|inner`; `main`, the default, is the screen in use, the one of the two that
  isn't black (the outer one when both are, so pass `inner` for a full-screen black app unfolded).
  The inner screen's pixels are its framebuffer's (2007×2853, the content on its side), and edges
  (`--edge`) are that framebuffer's too: home from the upright bottom is `--edge right`.
- **Touch.** Each screen has its own digitizer, addressed by its screen ID with the screen bit
  (`IndigoHIDTargetForScreen`). The main digitizer follows SpringBoard's primary screen, so after a
  fold it lands on the inner screen: on a device with two screens every touch names its screen.
  Other iPhones and iPads keep the main digitizer, unchanged.
- **The desktop** shows the screen in use and switches when the device folds; an iPhone Duo's
  toolbar gets Fold, Half open and Unfold.
- **Not yet:** the inner screen isn't turned upright (in screenshots or the desktop); `rotate` was
  not checked unfolded; powering a built-in screen off and on through the service
  (`setPowerState`) leaves it black until the device reboots, so simee never does.

## Apple TV

| Feature | tvOS 26.5 | tvOS 27.0 |
| --- | --- | --- |
| `parity.sh` | pass | pass |
| screenshot, frame callbacks | pass | pass |
| buttons (home, arrows, select, menu, play/pause), `remote-swipe`, type, key | pass | pass |
| launch, terminate, apps, location, memory warning, processes | pass | pass |
| network shaping: proxy and injected | pass | pass |
| audio: state, capture start/stop | pass | pass |
| appearance | unsupported⁴ | unsupported⁴ |
| open URL (`https://`) | unsupported⁵ | unsupported⁵ |
| rotate | unsupported (an Apple TV doesn't rotate) | unsupported |

4. The tvOS runtime has no user interface style: `simctl ui appearance` fails the same way, and
   simee now says why ("Runtime does not support userInterfaceStyle").
5. tvOS has no web browser to open the URL; `simctl openurl` fails the same way.

## Back-to-back installs (#123)

On 2026-11-26, on a heavily loaded host, about 100 back-to-back `install`s of the same app left an
iOS 27.2 iPhone 17 Pro unable to launch it: SpringBoard held the app as being updated
(`LaunchServicesDataMismatch`), each `launch` failed after 12.6 s of SpringBoard retries, and only a
reboot cleared it. It did not happen again on 2026-11-27 (M3 Max, macOS 26.6, Xcode 26.6, a fresh
iOS 27.2 iPhone 17 Pro, settled, host load 12 to 26) with the one-screen app `scripts/ios-matrix.sh`
builds (108 KB), in any of these loops:

| Loop | Installs | Install median / p90 / max | Launch afterwards |
| --- | --- | --- | --- |
| simee `install`, one kept API connection | 200 | 137 / 158 / 361 ms | ok, 37 ms |
| `xcrun simctl install` | 200 | 386 / 504 / 827 ms | ok, 45 ms |
| simee install, launch, terminate, repeated | 100 | 134 / 149 / 396 ms | all 100 ok |
| simee installs over the running app | 100 | | ok, 41 ms |
| simee installs while a second connection launches it (`restart: false`) without pause | 100 | | 100 overlapping launches ok (max 2.6 s); then ok, 0.56 s |

So there is no count at which it starts on an unloaded host (510 simee installs and 200 `simctl`
ones on one boot). SpringBoard's side of each install (`applicationsDidInstall`, then removing the
install placeholder, in its `com.apple.frontboard.ApplicationLibrary` log) was done 10 to 16 ms
before `install` returned, over 10 installs 0.5 s apart: there is nothing for `install` to wait
for, so it doesn't. The state is SpringBoard falling behind the install notifications (each
install registers the app with LaunchServices three times and wakes about 20 daemons), which a
loaded host makes likely.

What simee does about it: when SpringBoard refuses a launch as being updated (FrontBoard's code
`LaunchServicesDataMismatch`, or a reason saying the app "is being updated", anywhere in the
error's underlying errors), `launch` says so and that a reboot clears it, instead of passing on
CoreSimulator's generic "failed to launch". The API server remembers it per device and app: for
the next 60 s in the same boot (the device's `launchd_sim`), a launch of that app fails at once
with the same error instead of spending another 12.6 s. A reboot, or the minute passing, sends
the next launch to the device again. The first refusal still takes SpringBoard's 12.6 s.

Reproduce: `simee serve --socket <s>`, then over one connection send `install` 200 times and
`launch` once (`{"device": "<udid>", "path": "<Tiny.app>"}`, then `{"device": "<udid>",
"bundle_id": "dev.simee.tiny"}`); for `simctl`, loop `xcrun simctl install <udid> <Tiny.app>`.

## What changed in simee for these runtimes

- **Blocks carry signatures.** The screen adapter and framebuffer descriptors are ROCKit remote
  proxies under this CoreSimulator, and ROCKit throws an Objective-C exception (which aborts the
  process: Rust can't catch it) for a block without a signature. `carplay` aborted the API server
  on iOS 18.0, and `Display::watch_frames` (the desktop's frame clock) aborted on iOS 26.5. Every
  block simee hands the service now carries its signature (`rt::ObjBlock` and friends).
- **The shaper loads on iOS 18.** It linked `libdns_services`, which iOS 18 runtimes don't have:
  with `inject: true`, every process the device started afterwards, `launchctl` included, died in
  dyld, so the injection couldn't be undone without a reboot. The library is now weakly linked
  (the Network.framework resolver hook is skipped where it doesn't exist), and injection first runs
  one program with the shaper and refuses if it doesn't load.
- **The iPhone Duo's main screen.** It has two built-in framebuffers, and the inner one (black when
  folded) comes first; simee captured and touched that one. The main display is now the built-in
  framebuffer the size of the device type's main screen (`sim_core::framebuffer::main_port`).
  `parity.sh` asks `simctl` for the `primary` display. The inner screen and the hinge: see
  [iPhone Duo](#iphone-duo).
- **Network shaping on Apple TV.** The shaper and the proxy tool were built for the iOS simulator
  only, and dyld refused them in a tvOS device. They are built per platform now
  (`sim_netshape::Sdk`). watchOS and visionOS devices get a clear error.
- `audio_capture_stop` no longer resolves a default device (it failed with several booted).
- `rotate` refuses devices other than iPhone and iPad instead of reporting success.
