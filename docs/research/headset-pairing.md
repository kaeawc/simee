# Pairing headsets and glasses with phones

What "a headset and a phone" means on visionOS and on Android XR, which of those flows the
simulators and emulators can carry, what simee would add, and the follow-up work. Issue
[#426](https://github.com/kaeawc/simee/issues/426), part of the device pairing epic
[#429](https://github.com/kaeawc/simee/issues/429); the link model it feeds is
[#427](https://github.com/kaeawc/simee/issues/427)
([design/device-links.md](../design/device-links.md)).

- Written 2026-11-28 from Apple's and Google's documentation, Apple's developer forums and this
  repository's code. The VM it was written on has no KVM and no Mac: **nothing here was run**.
- Each finding is tagged **[verified]** (read in the cited page or this repo's code) or
  **[inferred]** (from a third-party report, a forum post or reasoning, not run). The follow-up
  work turns the inferred ones into checks.

## Summary

| | Apple Vision Pro (visionOS) | Android XR headset | Android XR audio and display glasses |
| --- | --- | --- | --- |
| Needs a phone to work? | no: standalone | no: standalone | **yes**: apps run on the phone and project to the glasses |
| Stock pairing flow for virtual devices | none | none | Android Studio's *Pair Glasses* assistant, one phone AVD per glasses AVD |
| Phone flows apps use | Handoff, Universal Clipboard, iPhone unlock, SharePlay, local-network apps | Samsung's Galaxy XR app (Galaxy phones only), Quick Share | Jetpack Projected (the phone app's activity on the glasses' display, sensors and audio) |
| What the simulator carries | local-network apps (Bonjour on the host); SharePlay with simulated participants only | local-network apps behind the emulator's NAT | the pairing itself, over the emulator's Bluetooth |
| What simee needs | nothing new for iOS-to-iOS networking; a `headset-phone` link with roles | the shared network (`net-forward`, then `net-segment`) | Bluetooth between emu devices (#435), the glasses image and a canary phone image booting on emu, the companion handshake |
| Link | `headset-phone`, mechanism `network` (host network) | `headset-phone`, mechanism `net-forward` / `net-segment` | `headset-phone`, mechanism `bt-netsim` |

The answer to the issue's three questions in one paragraph each:

- **visionOS** has no phone pairing at all, real or simulated: a Vision Pro is a peer of the
  user's iPhone through their Apple Account, and every flow between them (Handoff, Universal
  Clipboard, iPhone unlock) needs the account, Bluetooth and Wi-Fi of real hardware. The one flow
  the simulators carry is an app talking to its own copy on another device over the local
  network, and iOS simulators already share the host's network. A `headset-phone` link on iOS is
  a label with roles, not a mechanism.
- **Android XR headsets** don't expect a phone. **Android XR glasses do**: Google's glasses AVDs
  must be paired with a phone AVD, the app runs on the phone, and the pairing is a Bluetooth
  bond plus a companion-device association made by the phone's Glasses app. This is the one real
  pairing flow in the headset lane, and it is the same work as Wear OS over Bluetooth and the car
  lane: emu's Bluetooth port bridged to netsimd (#435).
- **The shared network** is the useful common denominator for headsets and phones (both
  platforms' headsets are standalone, and the multi-device apps on them talk over the local
  network), but simee doesn't give every device one reachable LAN today: iOS simulators share the
  host's network and see each other; each Android guest is alone behind its own NAT with no
  inbound connections and no multicast. `net-forward` (designed in #427) covers apps that are
  told an address; `net-segment` (not filed yet) is what makes discovery work between Android
  devices.

## 1. visionOS and an iPhone

### What a Vision Pro does with an iPhone

On real devices every flow between a Vision Pro and an iPhone rides on both being signed in to the
same Apple Account, near each other, with Bluetooth and Wi-Fi on. None is a pairing in the
watchOS sense: there is no bond, no companion app and no pair record.

| Flow | What it does | Needs | Source |
| --- | --- | --- | --- |
| Handoff | an `NSUserActivity` started on one device continues on the other | same Apple Account, Bluetooth, Wi-Fi, Handoff on (Vision Pro: Settings → General → Handoff) | [verified] Apple support 102430 lists Vision Pro and the requirements |
| Universal Clipboard | copy on one, paste on the other | same as Handoff | [verified] Apple support 102430 |
| Unlock iPhone while wearing Vision Pro | a Face ID iPhone unlocks with the headset on | visionOS 26 and iOS 26, same Apple Account, enabled in Settings | [verified] Apple's visionOS 26 announcement |
| SharePlay | a `GroupActivity` shared with other people's devices, over FaceTime or, from visionOS 26, with nearby Vision Pro wearers | FaceTime, or nearby Vision Pros with Window Sharing on | [verified] WWDC25 session 318 ("Share visionOS experiences with nearby people") |
| Local-network apps | an app and its iPhone or iPad copy find each other with Bonjour and talk over Network framework | the Local Network capability and the user's permission | [verified] Apple's sample "Connecting iPadOS and visionOS apps over the local network" (title and topic; its body was not readable from this VM) |
| Mac Virtual Display | a Mac's screen in the headset | a Mac, not an iPhone | [inferred] out of scope here |
| An iPhone as a keyboard or controller | no system flow found | — | [inferred] none in Apple's documentation that was searched |

Nearby SharePlay in visionOS 26 joins other Vision Pros in the same room; no source read says an
iPhone joins it as a nearby participant [inferred]. An iPhone joins a visionOS SharePlay session
through FaceTime, as any participant does.

### What the simulators carry

- **No pairing.** CoreSimulator's device pairs are watch-to-phone (`docs/design/device-links.md`,
  "iOS: CoreSimulator device pairs"); no source describes a visionOS-to-iPhone pair, and one
  would have nothing to carry [inferred: that CoreSimulator refuses a Vision Pro + iPhone pair is
  a check in the follow-ups].
- **No Handoff, Universal Clipboard or unlock.** Each needs an Apple Account on both devices and
  their Bluetooth; simulators have no Bluetooth radio, and no source describes Handoff between
  simulators [inferred].
- **SharePlay, partly.** Xcode 16 added simulated FaceTime calls; a simulated participant joins a
  call, but forum answers say it never joins the app's activity, so two-way messages can't be
  tested, and spatial Persona templates aren't supported in Simulator [inferred: Apple developer
  forums, threads 733637 and 791456]. Nearby sharing needs real headsets in one room [inferred].
- **Local-network apps, yes.** Simulator apps are host processes on the host's network stack
  (`crates/sim-netshape/src/lib.rs`) [verified], so a visionOS simulator and an iPhone simulator
  on one Mac reach each other on loopback and on the Mac's interfaces, and Bonjour runs through
  the host's mDNSResponder. One caveat: macOS 15.4 and 15.5 enforced local network privacy on
  simulator apps and failed `.local` lookups and `NWBrowser` with `PolicyDenied(-65570)`; Apple
  DTS called it a macOS bug (r. 148870123), and developers report it fixed in macOS 15.6
  [inferred: Apple developer forums, thread 780655]. Multipeer Connectivity uses Bonjour and had
  the same failure; it is also unavailable for some uses on visionOS and Apple recommends Network
  framework instead [inferred: threads 735245 and 829633].

### What simee would add on iOS

Nothing at the transport level: two iOS-family simulators already share a network. simee adds:

1. **The link record with roles** (`headset-phone`, mechanism `network`): the desktop draws the
   pair, `create` can make the iPhone as a companion, `boot {companions: "boot"}` boots both,
   and `link_status` reports both booted.
2. **A local-network check in `link_status`**: whether both members can resolve a Bonjour
   service the other advertises, so a user on an affected macOS learns that from simee rather
   than from their app. This needs a device to build and verify (follow-up 6).
3. **Shaping** is already per device (`network_shape`); shaping the link between two members
   only would need the injected shaper to know the peer's addresses, which on one host are the
   host's own. Not proposed until asked.

## 2. Android XR headsets and a phone

### What a headset expects

- Google's headset AVDs need no phone: Android Studio's guide for XR headsets and wired XR
  glasses creates one AVD and runs the app on it, with no host device step [verified: "Create
  virtual devices for XR headsets and XR glasses"]. The glasses guide is where the phone appears
  (section 3).
- Samsung Galaxy XR, the shipping headset, sets up and runs alone. Its phone features (calls,
  notifications, a battery view) come from Samsung's Galaxy XR app, available only on the Galaxy
  Store and for Galaxy phones, with the same Samsung account on both [verified: Samsung support
  ANS10007595, ANS10007510]. That app isn't in any SDK image, so an emulator can't carry it.
- Quick Share is listed among the headset's features, and Samsung's version needs both devices
  signed in to one Samsung account [verified: Samsung support ANS10007510]. Quick Share and the
  Nearby Connections API discover peers over Bluetooth, Wi-Fi and Wi-Fi Direct [inferred]; an
  emulator's Wi-Fi is a virtual access point with no peer-to-peer mode, and emu's Bluetooth has
  no peers yet ([bluetooth.md](bluetooth.md)), so neither works between emu devices today
  [inferred].
- Casting from a phone to a headset: no SDK flow found [inferred].

What remains for headsets is the same as for any two Android devices: an app on the headset and
an app on a phone that talk over the network.

### What emu carries today

- Each Android guest is alone at 10.0.2.15 behind emu's user-mode NAT, reaches the host's
  loopback at 10.0.2.2, and takes no inbound connection except through adb; `redir` isn't
  implemented [verified: `apps/emu/src/console.rs`, `crates/emu-vmm/src/net/`,
  `docs/design/device-links.md` "A shared virtual network"]. Multicast doesn't leave the guest,
  so mDNS (`NsdManager`) and Nearby don't cross between devices [verified, same sources].
- A headset can reach a phone's server only through a host forward (`adb -s <phone> forward`)
  dialled at 10.0.2.2, and an iOS simulator can reach it at 127.0.0.1 through the same forward.
  That is `net-forward` in the link design; it works with no emu change.
- Android XR isn't booted on emu yet ([form-factors.md](../form-factors.md), "Known gaps"), so
  none of this has been tried on an XR guest.

### What simee would add for headsets

1. **`headset-phone` over `net-forward`** (designed in #427): the link names the ports the
   members listen on, the adapter forwards them and `link_status` says which listen. No
   discovery: the app has to be told the address.
2. **`net-segment`** (not filed yet): a second NIC per linked Android member on a link subnet,
   frames relayed between the members' emu processes, multicast included. This is what makes
   mDNS and Nearby's Wi-Fi LAN medium work between a headset and a phone, and between any two
   Android devices. Stock has a similar idea for Wi-Fi (`-android-wifi-server-port`,
   `-android-wifi-client-port`, per `docs/design/device-links.md`).
3. **Quick Share and Nearby over Bluetooth** wait for #435 like everything Bluetooth; neither is
   proposed as its own work until #435 shows emu devices can see each other.

## 3. Android XR glasses and a phone

The `vision` form factor on Android covers the `ai-glasses` image (display glasses, API 36,
`ai_glasses_device`) as well as the headsets [verified: `crates/sim-core/src/form_factor.rs`,
`is_glasses_tag`; [form-factors.md](../form-factors.md)]. Glasses are where Android XR has a real
pairing.

### What stock does

- Audio glasses and display glasses AVDs "require a phone AVD to act as the host device for your
  app", one phone per glasses AVD: "A single phone AVD can't be paired to multiple audio or
  display glasses AVDs" [verified: "Create virtual devices for audio glasses and display
  glasses"].
- The phone AVD uses a pre-release image: "API CANARY Preview", `canary-20261007`, the
  "Pre-Release 16 KB Page Size Google Play" image (arm64-v8a on macOS) [verified, same page].
- Pairing: Device Manager → the glasses AVD's *Pair Glasses* → pick the phone; the Pairing
  Assistant starts both AVDs and starts pairing; the user accepts the association prompts on the
  phone. After the first pairing "the emulators automatically reconnect the devices to each other
  whenever you launch them" [verified, same page].
- The manual fallback runs on the phone: the glasses AVD shows "Pair phone to …"; on the phone,
  the **Glasses** app → *Set up glasses* → grant its permissions → *Begin Pairing* → pick the
  glasses AVD → accept the **Glasses Core** app's association request → *Next* [verified:
  "Troubleshoot Android XR Emulator issues for audio glasses and display glasses"].
- The link is Bluetooth plus a companion-device association: the fixes for a broken pairing are
  Settings → Connected devices → the glasses → *Forget device*, and wiping the glasses AVD
  [verified, same page]. Between two stock emulators Bluetooth runs through netsimd
  ([bluetooth.md](bluetooth.md), "What stock does"), so the glasses' pairing is a netsim bond
  [inferred: no page names netsim for glasses].
- Apps run on the phone: "select the phone AVD from the target device drop-down … and then click
  Run"; a *projected activity* (Jetpack Projected) shows on the glasses [verified: "Run your app's
  augmented experiences on the emulator"]. The projection service is
  `com.google.android.glasses.core/com.google.android.projection.core.app.service.AndroidProjectionCoreService`,
  which takes preferences through `adb shell dumpsys activity service … preferences_set`
  (`pref_automatic_snooze_timeout`) [verified, same page]. Whether that runs on the phone or the
  glasses, and what carries the projected frames (Bluetooth, a socket forwarded by the
  assistant, or Wi-Fi) isn't documented [inferred: open question 1].
- Requirements at the time of DP3 (December 2025): Android Studio Otter 3 Canary 4 or later and
  emulator 36.4.3 Canary or later [verified: Android Developers Blog, "Build for AI Glasses with
  the Android XR SDK Developer Preview 3"].

### What emu needs

| Need | Status in emu | Work |
| --- | --- | --- |
| The `ai-glasses` image boots | not booted; offline check of its fstab, modules, input names and touchpad done ([form-factors.md](../form-factors.md)) | boot it (follow-up 2) |
| The canary phone image boots | not tried; 16 KB pages | boot it; see whether emu's guest RAM and the kernel's page size need anything (follow-up 2) |
| Bluetooth between the two | a controller with no peers (`crates/emu-vmm/src/bt/`) | #435's netsimd bridge |
| A stable Bluetooth address per device | `address_for` today | the saved address the link design asks for (device-links open question 14) |
| The association without taps | stock needs taps on the phone | AOSP's companion device shell (`cmd companiondevice`, which has an `associate` subcommand for tests) may make it without UI [inferred: to check on the image, follow-up 3] |
| Reconnect on boot | stock does it | rebuilt by the link store's boot hook, as for Wear OS (#445) |

### What simee would expose

`link {phone, glasses}` with `kind: headset-phone` (inferred from the `vision` + `phone` form
factors), mechanism `bt-netsim`, the headset member's role `headset`. Until #435 lands, the
Android adapter refuses a glasses member with that reason instead of making a link that can't
become active, as it does for `car-phone` (`docs/design/device-links.md`). An Android XR headset
member gets `net-forward` instead. One phone may be in at most one glasses link, as stock has it.

## The cross-platform baseline: one link network

Both platforms' headsets are standalone, and the multi-device apps written for them (a headset
app with a phone or tablet controller, a shared session, a second screen) talk over the local
network. So a shared network between linked devices is the common denominator, and the only one
that works across platforms. What each mechanism gives:

| Between | Today | `net-forward` (#427) | `net-segment` (to file) | mDNS reflector (not proposed) |
| --- | --- | --- | --- | --- |
| iOS ↔ iOS | everything: same host stack | — | — | — |
| Android → iOS or host | outbound to 10.0.2.2 | — | — | — |
| iOS or host → Android | adb forward only | named ports, no discovery | — | — |
| Android ↔ Android | nothing direct | named ports via the host, no discovery | one L2 segment, mDNS and Nearby (Wi-Fi LAN) | — |
| iOS ↔ Android discovery | no | no | no | Bonjour both ways |

Not every device simee boots has one reachable LAN today: iOS devices do, Android devices don't,
and across the two only forwarded ports work.

## Recommended order

1. **The link record** (#427): `headset-phone` over the host network for iOS and `net-forward`
   for Android XR headsets. No new transport, and it gives the desktop (#428) the pair to draw.
2. **Boot the glasses image and the canary phone image on emu** (follow-up 2): needed before
   any glasses pairing, and cheap to learn early.
3. **Glasses pairing over Bluetooth** (follow-ups 3 and 4), after #435.
4. **`net-segment`** (follow-up 5), when a user needs discovery between Android devices; it
   serves every `network` link, not only headsets.
5. The visionOS checks (follow-up 6) whenever a Mac is at hand; they change only what
   `link_status` reports.

Not yet measured: nothing here moves a published number. A glasses pair's boot time and
netsimd's cost per device go under "Not yet measured" in `docs/benchmarks.md` with #435.

## Open questions

1. **What carries Jetpack Projected's frames** between the phone and the glasses emulator, and
   does the Pairing Assistant add adb forwards as Wear OS's does? Read Android Studio's glasses
   pairing code, or capture `adb forward --list` and netsim's devices during a stock pairing.
2. **Does emu need anything for 16 KB page images?** The guest kernel picks its page size; emu's
   guest RAM is mapped in host pages (16 KB on Apple silicon), so likely not, but untested.
3. **Is there one role name for headsets and glasses?** Default: both are `headset`, as the
   `vision` form factor covers both; the desktop can draw glasses differently from the image tag.
4. **iOS link check**: is a Bonjour round trip in `link_status` worth its cost, or is a
   documented macOS minimum enough? Default: document macOS 15.6 or later, add the check only if
   users hit it.

## Follow-up work

Not filed from this lane (it can open one PR and post one comment on #426); each is ready to file
as written, with the labels shown.

| # | Title | Scope | Labels |
| --- | --- | --- | --- |
| 1 | pairing: headsets — `headset-phone` links on the host network (iOS) and `net-forward` (Android XR headsets) | the adapter mapping in #427's model, roles, refusal of glasses members until #435 | enhancement, area:api |
| 2 | emu: boot the `ai-glasses` image and the canary 16 KB Play phone image | boot both, record what breaks in `docs/android.md`, `docs/form-factors.md` | enhancement, area:emu, needs-device |
| 3 | spike: Android XR glasses pairing on stock — transport, apps, forwards, and `cmd companiondevice associate` without taps | answers open question 1, sets follow-up 4's scope | research, needs-device |
| 4 | pairing: Android XR glasses with a phone over Bluetooth (`bt-netsim`, after #435) | `link`, reconnect on boot, `link_status`, an e2e test | enhancement, area:emu, needs-device |
| 5 | emu: `net-segment`, one L2 segment with multicast between linked Android devices | second NIC, relay between emu processes, mDNS and Nearby (Wi-Fi LAN) between guests, measured idle cost | enhancement, area:emu |
| 6 | ios: verify a visionOS and an iPhone simulator on one Mac — Bonjour both ways, CoreSimulator refusing a Vision Pro + iPhone pair, SharePlay with simulated participants | rows in `docs/ios-versions.md`, the macOS minimum in the link docs | research, area:ios-sim, needs-device |

Deliberately not proposed: an iOS-to-Android mDNS reflector (no user yet; device-links open
question 8), Galaxy XR's phone app (Samsung-only, not in any image), and Handoff or Universal
Clipboard between simulators (they need an Apple Account and Bluetooth the simulators don't have).

## Sources

Read 2026-11-28.

- [Create virtual devices for audio glasses and display glasses](https://developer.android.com/develop/xr/jetpack-xr-sdk/run/create-avds/glasses):
  the phone host AVD, its canary image, one phone per glasses AVD, the Pairing Assistant.
- [Troubleshoot Android XR Emulator issues for audio glasses and display glasses](https://developer.android.com/develop/xr/jetpack-xr-sdk/run/emulator/glasses-troubleshoot):
  manual pairing with the Glasses app, the Glasses Core association, forgetting the Bluetooth
  device.
- [Run your app's augmented experiences on the emulator](https://developer.android.com/develop/xr/jetpack-xr-sdk/run/emulator/glasses):
  running on the phone, the projection service, audio-only mode.
- [Create virtual devices for XR headsets and XR glasses](https://developer.android.com/develop/xr/jetpack-xr-sdk/run/create-avds/xr-headsets-glasses):
  headsets with no host device.
- [Android XR device types](https://developer.android.com/develop/xr/devices): headsets, wired XR
  glasses, audio and display glasses.
- [Build for AI Glasses with the Android XR SDK Developer Preview 3](https://android-developers.googleblog.com/2025/12/build-for-ai-glasses-with-android-xr.html):
  Studio and emulator versions, Jetpack Projected.
- Samsung support: [Use the Galaxy XR app on your Galaxy phone](https://www.samsung.com/us/support/answer/ANS10007595/),
  [Galaxy XR: connect with other Samsung devices and services](https://www.samsung.com/us/support/answer/ANS10007510/).
- [Use Universal Clipboard](https://support.apple.com/102430): the devices, Vision Pro included,
  and the requirements shared with Handoff.
- [visionOS 26 introduces powerful new spatial experiences](https://www.apple.com/newsroom/2025/06/visionos-26-introduces-powerful-new-spatial-experiences-for-apple-vision-pro/):
  unlocking iPhone while wearing Vision Pro.
- [WWDC25: Share visionOS experiences with nearby people](https://developer.apple.com/videos/play/wwdc2025/318/):
  nearby SharePlay and the Share menu.
- [Connecting iPadOS and visionOS apps over the local network](https://developer.apple.com/documentation/visionos/connecting-ipados-and-visionos-apps-over-the-local-network)
  (title and topic only; the page body didn't render for this VM).
- Apple developer forums, tagged inferred where used: threads
  [733637](https://developer.apple.com/forums/thread/733637) (SharePlay in the visionOS
  Simulator), [791456](https://developer.apple.com/forums/thread/791456) (WWDC25 visionOS group
  lab), [780655](https://developer.apple.com/forums/thread/780655) (Bonjour `PolicyDenied` in
  Simulator, r. 148870123), [735245](https://developer.apple.com/forums/thread/735245) and
  [829633](https://developer.apple.com/forums/thread/829633) (Multipeer Connectivity).
- This repository: [design/device-links.md](../design/device-links.md),
  [bluetooth.md](bluetooth.md), [wearos-pairing.md](wearos-pairing.md),
  [car-pairing.md](car-pairing.md), [form-factors.md](../form-factors.md),
  `crates/sim-core/src/form_factor.rs`, `crates/sim-netshape/src/lib.rs`,
  `crates/emu-vmm/src/net/`, `crates/emu-vmm/src/bt/`, `apps/emu/src/console.rs`.
