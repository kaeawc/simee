# Pairing cars with phones

Three different things are called "a car and a phone": Android Auto (the phone runs the app, a head
unit shows it), Android Automotive OS (the car runs Android and a phone pairs with it over
Bluetooth) and CarPlay (the iPhone simulator's own external display). What each needs, what can be
automated, what simee would expose, and in which order to build it. Issue
[#425](https://github.com/kaeawc/simee/issues/425), part of the device pairing epic
[#429](https://github.com/kaeawc/simee/issues/429); the link model it feeds is
[#427](https://github.com/kaeawc/simee/issues/427).

- Written 2026-11-28 from documentation, package manifests and this repository's code. The VM it
  was written on has no KVM and no Mac: **nothing here was run**, and no SDK package was
  downloaded (the DHU's manifest entry was read, not its archive).
- Each finding is tagged **[verified]** (read in the cited page, the manifest or this repo's
  code) or **[inferred]** (from a third-party report or reasoning, not run). The follow-up issues
  turn the inferred ones into checks.

## Summary

| | Android Auto (DHU) | Android Automotive + phone | CarPlay |
| --- | --- | --- | --- |
| What the "car" is | Google's Desktop Head Unit, a host program | an `android-automotive` emu device (the `auto` form factor) | the iPhone simulator's external screen |
| The link | `adb forward` to the phone's head unit server | Bluetooth (HFP, PBAP, MAP, A2DP, AVRCP) between two emu devices | none: one device, two screens |
| Needs from simee | provision the DHU, spawn and supervise it, a console client | a virtual Bluetooth link between emu devices, a booted car image | list and show the existing display |
| Blocker | starting the head unit server has no documented command | the link doesn't exist; the car image has never been booted | Apple: Xcode 27 needs a real iPhone |
| Feasible without new infrastructure | mostly (host process orchestration) | no (needs the netsim bridge) | yes, on iOS 26 runtimes |
| Exposure | a car display on the phone: `display: "car"` | `link {car, phone, kind: car-phone}` | the same car display, `carplay` kept |
| Order | 2 | 3 | 1 |

Recommended order: **CarPlay's surface first** (it is already implemented and only needs a Mac
check and a listing), **Android Auto second** (the most used car platform for app developers and
buildable from the host side), **Android Automotive with a phone last** (it waits for the
Bluetooth link, which the Wear OS lane
[#424](https://github.com/kaeawc/simee/issues/424) also needs, and for a car image to boot at
all). Details in [Recommended order](#recommended-order).

## 1. Android Auto through the Desktop Head Unit

### How the DHU connects

The DHU is a host program that plays a head unit; the phone runs the Android Auto app, which runs
the projected apps. Google documents two transports: USB accessory mode (`desktop-head-unit
--usb`, the recommended one for DHU 2.x) and ADB tunnelling. **[verified]** (DHU page)

An emulated phone has no USB, so only the tunnel applies:

1. Android Auto's developer mode is on (open Android Auto's settings, tap the version row ten
   times, confirm "Allow development settings?").
2. In Android Auto's overflow menu, "Start head unit server". A foreground service notification
   shows it's running.
3. "Add new cars to Android Auto" is enabled under "Previously connected cars", and the phone's
   screen is unlocked.
4. On the host: `adb forward tcp:5277 tcp:5277`, then `./desktop-head-unit` from
   `<sdk>/extras/google/auto/`. `--adb=[host:]port` changes the port the DHU dials.
5. On the first connection the phone shows the terms of service and permission prompts.

**[verified]** (DHU page, "Connect using ADB Tunneling" and "Command-line options"). Two facts
shape the design: the forward is per adb device, so several emulated phones each get their own
free host port (`adb -s emulator-5556 forward tcp:<free> tcp:5277`, `--adb=<free>`); and Google
notes a blank DHU on first connect is fixed by restarting the head unit server.

### Can it be automated end to end?

Almost. Two steps are not commands in Google's documentation:

| Step | Documented as | Automatable? |
| --- | --- | --- |
| Install and update Android Auto on the phone | Play Store; Android 10+ also needs a Play sign-in and an update before the first DHU run | **[verified]** it's a requirement; whether emu's images can satisfy it is open |
| Developer mode, "Start head unit server" | menu taps | no documented command. A third-party report starts the service directly as root: `am startservice -W com.google.android.projection.gearhead/com.google.android.projection.gearhead.companion.DeveloperHeadUnitNetworkService` **[inferred]**: unverified against the current app. `google_apis` images are userdebug, so `adb root` may be enough. Fallback: simee's own `tap`/`swipe` drive the menu once per boot |
| `adb forward`, spawn the DHU | commands | yes |
| Terms of service and permissions on first connect | phone screen | once per AVD, by taps |
| Restart the server after every cold boot | | one report says it's needed after each cold boot of an emulator **[inferred]**; a snapshot may keep it |

The DHU itself is scriptable, which is what makes a console client possible **[verified]**:

- Its commands come from stdin and `sleep [seconds]` exists "for scripting the head unit with
  `./desktop-head-unit < script.txt`, for example in a CI environment".
- `tap x y`, `keycode home|back|call|search|media_*|navigation|tel`, `dpad ...` (rotary),
  `screenshot file.png`, `day|night|daynight`, `focus video|audio|nav on|off`, `restrict
  none|all`, `mic play file.wav`, and the sensors (`speed`, `gear`, `location`, `fuel`,
  `parking_brake`, `odometer`, ...), which only work if the `.ini`'s `[sensors]` section enables
  them.
- `-i touch|rotary|hybrid`, `-c file.ini` (resolution 800x480, 1280x720 or 1920x1080, dpi,
  frame rate, margins, instrument cluster, a second cluster `[display]`, `windowleft/windowtop`),
  `-h/--headless` ("no UI"), `-t` (always on top).

Not documented: whether `--headless` still renders so that `screenshot` works, and a screenshot's
latency. That is the first thing to measure ([#438](https://github.com/kaeawc/simee/issues/438)).

### What the DHU needs, and the terms

- **Package**: `extras;google;auto`, "Android Auto Desktop Head Unit Emulator". The repository
  manifest (`repository2-3.xml`, read 2026-11-28) lists r2.0 (stable channel) and r2.1 (beta
  channel) with archives for macOS aarch64, macOS x64, Linux x64 and Windows x64, about 6.8 to
  8 MB each, each with a size and sha1; no Linux arm64 build. **[verified]** (manifest)
- **Install path**: `<sdk>/extras/google/auto/`; `chmod +x desktop-head-unit`; the bundled
  `libusb` stays beside it. Linux needs glibc 2.32 and `libc++1`, `libc++abi1`. **[verified]**
  (DHU page)
- **Licence**: the manifest's `uses-license` is `android-sdk-license`, the same as platform-tools
  and system images. The [SDK terms](https://developer.android.com/studio/terms) grant a licence
  "solely to develop applications for compatible implementations of Android" (3.1) and say you
  may not "copy ..., modify, adapt, redistribute ... the SDK or any part of the SDK" (3.4), with
  open source components under their own licences (3.5; `desktop-head-unit --licenses` lists the
  DHU's). **[verified]** (terms, manifest). This is a reading of the text, not legal advice.
- **So**: simee does not bundle or mirror the DHU. It downloads it from Google's repository
  when the user asks, verifies size and sha1, and unpacks it into the SDK, which is how
  `crates/emu-sdk` already treats system images and the renderer. Nothing is fetched otherwise.
  The Android Auto app is a Google Play app under its own terms; simee doesn't fetch it, and a
  user installs it with `install` ([#436](https://github.com/kaeawc/simee/issues/436)).
  *Default, owner may override.*

### Showing the DHU as a device or window

| Option | What simee gets | Cost and risk |
| --- | --- | --- |
| A. The DHU's own window | nothing in simee's UI, but it works at once; `windowleft/windowtop` can place it | no capture or input from simee |
| B. The DHU console over stdin/stdout | input (`tap`, `keycode`, `dpad`, sensors) and PNG screenshots, as the phone's second display | polling rate, not video; `--headless` plus `screenshot` unverified |
| C. Capture the window | live frames | a macOS Screen Recording permission per host, fragile window lookup |
| D. Speak the head unit protocol in simee | no DHU at all | a proprietary protocol and Google's terms for Android Auto; not pursued |

Take **A + B**: simee spawns and supervises the DHU (PID in `<avd>/simee/`, as for `emu`), and
treats the console as the control plane. The car then isn't a new device; it is the phone's
*car display*: `screenshot`, `tap`, `swipe` take `display: "car"` exactly as CarPlay takes
`display: "carplay"` (the API already has a `display` parameter for the iPhone Duo's two screens
and CarPlay). A `vehicle` call maps `speed`, `gear`, `night`, `parking-brake` onto the DHU's
commands, the same names as the Automotive `vehicle` method.
Video can come later, if a measurement says polling isn't enough.

### Steps and risks

1. Provision the DHU on request ([#436](https://github.com/kaeawc/simee/issues/436)).
2. Spike the phone side, on a Mac ([#438](https://github.com/kaeawc/simee/issues/438)).
3. Console client, port allocation and the API ([#437](https://github.com/kaeawc/simee/issues/437)).

| Risk | Why |
| --- | --- |
| The Android Auto app may not run on emu's images | one third-party report: a Play emulator image's stock Android Auto says "This app isn't compatible with your device anymore" until a newer APK is installed **[inferred]** |
| The head unit server can't start without taps | no documented command; the root route is unverified |
| Host support | DHU: macOS (both archs) and Linux x64 only **[verified]** |
| DHU 2.x is Google's to change | the beta (2.1) adds cluster display; an update can change the console |
| Terms | the DHU can't be redistributed; the user fetches it |

## 2. Android Automotive OS with a phone over Bluetooth

### What AAOS expects of a phone

AOSP's Bluetooth page for Automotive describes the car as the *client* side of each profile
**[verified]** ([source](https://source.android.com/docs/automotive/ivi_connectivity)):

| Profile | The car is | Needs from the phone |
| --- | --- | --- |
| HFP | hands-free client; each phone becomes a Telecom phone account (`HfpClientConnection`) | audio gateway; call audio goes over SCO, routed by the app through `IBluetoothHeadsetClient` |
| PBAP | client; contacts and call history into the Contacts Provider | server; the connection must be authorized on both ends, and the car initiates it |
| MAP | client; SMS in and out, broadcast as intents, not stored | server; authorized on both ends |
| A2DP | sink (one device; the limit is in the native stack) | source |
| AVRCP | controller; browse and control players | a `MediaBrowserService` the Bluetooth app can reach |

`CarBluetoothService` keeps per-profile priority lists; `BluetoothDeviceConnectionPolicy` decides
when to connect (adapter on, power on); `connectDevices()` does it. Verification on the page:
`dumpsys car_service` and toggling the adapter with `svc bluetooth`. Even Google's own AAOS image
for Pixel Tablet lists "HFP and A2DP may be missing or not fully functional" among its known
issues **[verified]**
([source](https://developer.android.com/training/cars/testing/aaos-on-pixel)), so a profile
failing is not necessarily emu's fault.

Which profiles *emu's* image enables, and its connection policy, aren't in these pages: read
them from a booted image ([#439](https://github.com/kaeawc/simee/issues/439)).

Android Auto's "receiver" mode (a phone projecting onto an Automotive device) is documented only
for the Pixel Tablet's Automotive build, over Bluetooth then Wi-Fi, with wired connections
unreliable **[verified]**; it isn't on the SDK's emulator images. An AAOS emulator is the car
with Google built-in, not an Android Auto head unit.

### What a virtual Bluetooth link between two emu instances needs

Today: emu's `bluetooth` port is a built-in BT 4.2 controller with **no peers**
(`crates/emu-vmm/src/bt/`: `h4.rs`, `hci.rs`, `controller.rs`, wired in `machine.rs`). The stack
runs, discovery finds nothing, pages time out, and the controller has no SCO, LE encryption,
privacy or extended advertising; `docs/research/bluetooth.md` has the full account. **[verified]**
(code, doc). Pairing a car and a phone needs, between two controllers:

- inquiry and inquiry results, page and page scan, and connection setup (create connection,
  accept, connection complete);
- the link-manager side of Secure Simple Pairing (IO capabilities, key agreement, link key),
  then authentication and encryption;
- ACL data relay in both directions (L2CAP, RFCOMM and the profiles are host-stack code on top);
- SCO for call audio; role switch and sniff, which stacks use;
- LE advertising and scanning for the stacks' LE half.

That's a second Bluetooth controller model with a radio between devices. The stock emulator gets
it from `netsimd`, which embeds rootcanal and links the emulators of one host user (and, by that
design, every emulator of that user sees every other unless started with a private daemon)
**[verified]** (`bluetooth.md`; Google's Wear OS docs describe the same emulated Bluetooth with
`-packet-streamer-endpoint default`,
[source](https://developer.android.com/training/wearables/apps/test-bluetooth-audio)).
`bluetooth.md` already recommends bridging emu's port to a netsimd (option 2) and not building a
controller model of our own (option 3), and what is unverified there is still unverified: that a
raw HCI connection shows up as a device other devices see, and the address netsimd assigns.

**simee needs, then**: the opt-in netsim bridge, a netsimd per link group (so unlinked devices
stay invisible), the car image booted, and the phone's side authorized. The bridge is shared work:
the Wear OS lane (#424) needs the same link, so it is its own issue
([#435](https://github.com/kaeawc/simee/issues/435)) and the car lane waits for it.

### What simee would expose

The car is an ordinary device (platform Android, form factor `auto`), so the new thing is the
link, in the model of #427: `link {devices: [car, phone], kind: "car-phone"}`, with `pair {car,
phone}` as the CLI spelling. Starting it brings up the link group's netsimd, boots the phone first,
bonds the two, and authorizes PBAP and MAP for the car. `links` shows state (pending, active,
broken), and the link is saved with both devices so it survives a reboot.

### Steps, risks, and what works without a link

1. The netsim bridge, measured (#435).
2. Boot a car image on emu: the VHAL channel and sound card exist but "needs a real boot"
   (`docs/android.md`, [Automotive](../android.md#automotive)).
3. Read the image's profiles and policy; link; check each profile with a fixture: a call from the
   phone's modem reaches the car's Telecom account, contacts over PBAP, an SMS over MAP, media
   over A2DP and AVRCP ([#439](https://github.com/kaeawc/simee/issues/439)).

| Risk | Why |
| --- | --- |
| No call audio | needs SCO through the other end's controller; **[inferred]** that rootcanal carries it, unchecked |
| Cost | netsimd's CPU and memory per device are unmeasured (`bluetooth.md`); don't make it the default before they are |
| The car image has never booted on emu | EVS, the cluster display and the rotary controller are not modelled |
| Hardware profiles are Google-defined | AOSP's page says some limits are in the native stack (A2DP: one device) |

Most car *apps* (media, navigation, templated apps) need no paired phone on Automotive: the
`vehicle` method drives the VHAL and the apps run on the car. Pairing matters for calling and
messaging flows, and for the contacts and media a phone supplies.

## 3. CarPlay with an iOS simulator

### What the simulator offers

- **Through Xcode 26 / iOS 26 runtimes** **[inferred]** (older guides and forum threads, not
  run here): Simulator.app's I/O, External Displays, CarPlay opens a
  window on the iPhone simulator's external screen; the app appears there only if its build has the
  CarPlay entitlement **[verified]** (Apple's page). Forum threads report a simple tool with a
  disappearing knob, an unreliable audio state and a window that sometimes ignored touches.
  Apple's own guidance is that the simulator is a first check and a real vehicle or
  head unit the realistic one
  ([Apple: Testing your app in CarPlay Simulator](https://developer.apple.com/documentation/carplay/testing-your-app-in-carplay-simulator)).
- **Xcode 27**: that page says CarPlay Simulator needs an iPhone connected over USB and is
  opened from Device Hub (Device, CarPlay Simulator): a window imitating the primary display with
  size and resolution options, environment settings and simulated vehicle controls **[verified]**.
  The standalone CarPlay Simulator app from Additional Tools is the fallback. A feedback report
  (FB24785359, 2026-09-15, still open when read) says Device Hub's CarPlay plugin asks for an
  internal-only entitlement, so a *simulated* iPhone has no CarPlay window in public builds
  **[inferred]**
  ([report](https://github.com/feedback-assistant/reports/issues/842)). simee's own docs already
  refuse iOS 27 runtimes ("iOS Simulator is not supported for CarPlay"; `sim_core::carplay`,
  [ios-versions.md](../ios-versions.md)).

### Can it be driven and captured, and how `carplay` fits

simee reaches the car screen without Simulator.app **[verified]** (code, not run on a Mac):
`carplay on` writes the StarkConfig (`extended_display.plist`: 800x480 at 2x, touch), sets the
iPhone's TVOut screen to that mode with CarPlay properties, and creates the CarPlay HID service;
touches go to HID target 1; `screenshot`, `tap`, `swipe`, `touch` take `display: "carplay"`
(`crates/sim-core/src/carplay.rs`, `crates/sim-coresim/src/carplay.rs`, `docs/api.md`). It has
never run (`docs/form-factors.md`: "no Mac was available"), and the knob, back, home and Siri
inputs aren't sent.

CarPlay is therefore not a pairing and not a second device. The car display is the phone's own
external screen, so the right shape is the same car display Android Auto gets: listed under its
phone in `devices`, shown beside it in the desktop, `display: "car"` as an alias of `carplay`.
Whether `sim-encode`'s `IosSource` (the main framebuffer's IOSurface) can stream the external
screen is unmeasured.

### Steps and risks

1. Verify `carplay on` on a Mac with an iOS 26 runtime, then list and show the display
   ([#440](https://github.com/kaeawc/simee/issues/440)).
2. Re-check Device Hub and the iOS 27 runtimes on each Xcode release
   ([#441](https://github.com/kaeawc/simee/issues/441)).

| Risk | Why |
| --- | --- |
| Apple removes it | Xcode 27 already needs a physical iPhone, and iOS 27 simulators render CarPlay black; the private screen path is Apple's to close |
| Never run | the implementation is read-only verified |
| Entitlement | an app needs the CarPlay entitlement and a provisioning profile that carries it to appear |
| Inputs | the knob and head unit buttons are unknown to SimulatorKit as far as we know |

## What simee would expose, together

| | Surface | Link |
| --- | --- | --- |
| CarPlay | the phone's car display, `display: "carplay"` or `"car"` | none (one device) |
| Android Auto | the phone's car display, `display: "car"`; `car_start` / `car_stop` | a supervised DHU process and an adb forward, recorded as a `projection` link |
| Android Automotive | a normal `auto` device | `link {car, phone, kind: car-phone}` over Bluetooth |

This keeps two ideas apart: a **projection** (the phone draws a car display; CarPlay and Android
Auto) and a **pairing** (two devices, a radio link; Automotive and the watch lanes). #427's `link`
can carry both with `kind` and a `via` (`bluetooth`, `adb`, none), and the desktop can draw the
projection as a second screen on its phone and the pairing as a tether.

## Recommended order

1. **CarPlay's surface** (#440, #441): no new backend, one Mac check, one listing change, and it
   tells the link model what a projection looks like before Android Auto needs it.
2. **Android Auto** (#436, then the spike #438, then #437): provisioning and the console client
   are code-only and testable on Linux with fakes; the spike is the one real unknown. Do the spike
   early, because its answer sets #437's scope.
3. **Android Automotive with a phone** (#435, then #439): the Bluetooth link is a prerequisite
   shared with Wear OS; the car image's first boot can start alone, at any point.

Not yet measured: the DHU's idle CPU and memory, `--headless` plus `screenshot` latency, netsimd
per device, and any of the three paths' effect on boot time. None is published; each issue says
where to put its row in `docs/benchmarks.md`.

## Follow-up issues

| Issue | Scope | Labels |
| --- | --- | --- |
| [#435](https://github.com/kaeawc/simee/issues/435) | the netsim bridge for emu's Bluetooth port, opt-in, measured | enhancement, area:emu |
| [#436](https://github.com/kaeawc/simee/issues/436) | provision the DHU on request (`extras;google;auto`) | enhancement, area:emu, cloud-ready |
| [#437](https://github.com/kaeawc/simee/issues/437) | DHU console client, supervisor, `car_start`, `car_stop`, `display: "car"` | enhancement, area:api |
| [#438](https://github.com/kaeawc/simee/issues/438) | spike: Android Auto on an emu phone, the head unit server without taps | research, needs-device |
| [#439](https://github.com/kaeawc/simee/issues/439) | boot an Automotive image, read its profiles, link it to a phone | enhancement, area:emu, needs-device |
| [#440](https://github.com/kaeawc/simee/issues/440) | verify CarPlay on iOS 26, list and show it with its phone | enhancement, area:ios-sim, needs-device |
| [#441](https://github.com/kaeawc/simee/issues/441) | re-check Xcode 27's CarPlay each release | research, upstream |

## Sources

Read 2026-11-28.

- [Test using the Desktop Head Unit](https://developer.android.com/training/cars/testing/dhu):
  install, `--usb`, ADB tunnelling, flags, console commands, `.ini` options.
- [Test your app for cars](https://developer.android.com/training/cars/testing): the tools
  overview and Android Auto's developer mode.
- [Android Automotive OS emulator](https://developer.android.com/training/cars/testing/emulator):
  system images and hardware profiles.
- [Android Automotive OS on Pixel Tablet](https://developer.android.com/training/cars/testing/aaos-on-pixel):
  Android Auto receiver mode and the Bluetooth known issue.
- [Bluetooth in Android Automotive](https://source.android.com/docs/automotive/ivi_connectivity):
  profiles, policy, phone-side requirements.
- [Test Bluetooth audio on emulators (Wear OS)](https://developer.android.com/training/wearables/apps/test-bluetooth-audio):
  emulated Bluetooth, `-packet-streamer-endpoint`.
- [Android SDK licence](https://developer.android.com/studio/terms): sections 3.1 to 3.5.
- [SDK repository manifest](https://dl.google.com/android/repository/repository2-3.xml):
  `extras;google;auto` r2.0 and r2.1 (the manifest only).
- [Apple: Testing your app in CarPlay Simulator](https://developer.apple.com/documentation/carplay/testing-your-app-in-carplay-simulator):
  the Xcode 27 flow. Apple's CarPlay Developer Guide (PDF) was not opened (over the fetch size
  limit); statements that depend on it are tagged inferred.
- [Feedback report FB24785359](https://github.com/feedback-assistant/reports/issues/842): Device
  Hub's CarPlay, third-party.
- Third-party, tagged inferred where used: a write-up of running Android Auto on an emulator
  ([helw.net](https://helw.net/2026/03/07/running-android-auto-on-an-emulator/)); a forum report
  of a root command that starts the head unit server.
- This repository: `docs/research/bluetooth.md`, `docs/form-factors.md`, `docs/android.md`
  ("Automotive"), `crates/emu-vmm/src/bt/`, `crates/emu-vmm/src/pipe/car.rs`,
  `crates/sim-core/src/carplay.rs`, `crates/sim-coresim/src/carplay.rs`, `crates/emu-sdk`.
