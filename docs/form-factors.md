# Form factors

Every device has a platform (`android` or `ios`), a form factor and the sub-platform that pairs
them. The model is `sim_core::form_factor`; `simee form-factors` and the API's `form_factors`
print it. Which iOS and tvOS runtime versions each feature works on: [ios-versions.md](ios-versions.md).

## Support

| Form factor | Android | iOS (Apple) |
| --- | --- | --- |
| `phone` | Android | iOS |
| `tablet` | Android | iPadOS |
| `foldable` | Android | iOS (the iPhone Duo, iOS 27.1 runtimes on) |
| `watch` | Wear OS | watchOS |
| `tv` | Android TV | tvOS |
| `auto` | Android Automotive | CarPlay |
| `vision` | Android XR | visionOS |

Every pair exists. iOS's foldable is the iPhone Duo (the iOS 27.1 and `iOS-27-2-iPhone19-4`
runtimes): `fold` and `display=outer|inner` drive its hinge and its two screens, see
[ios-versions.md](ios-versions.md#iphone-duo). A device type that isn't the form factor asked for
is refused with the reason:

```text
$ simee create x "iPhone 17" "iOS 26.5" --form-factor foldable
Error: device type `iPhone 17` is a phone, not a foldable
```

`auto` means different things on each side, and the labels say which: on Apple it is **CarPlay**,
the car display an iPhone simulator projects onto; on Android it is **Android Automotive OS**, the
OS running on the head unit itself (what the emulator's automotive images are). Android Auto, which
projects from a phone, isn't an emulator image. Linking a car to a phone (Android Auto's head unit,
Automotive over Bluetooth, CarPlay's display) is researched in
[research/car-pairing.md](research/car-pairing.md).

`vision` is a headset on both sides: **visionOS** on Apple, **Android XR** on Android. Google
publishes arm64 Android XR images that run on Apple silicon, from the `google_xr` repository site
(`sys-img/google_xr/sys-img2-*.xml`, listed in the SDK's site list): `google-xr`,
`android-xr-v3-playstore` and `android-xr-preview-playstore` (headsets, the last also XR glasses;
all carry the `android-xr` tag) at API 34, and `ai-glasses` (display glasses) at API 36. All are
developer previews.

## Reference profiles

What a new device of each kind gets unless told otherwise. Real devices report their own screen.

| Kind | Screen (px) | Density | Shape | Fold | Inputs |
| --- | --- | --- | --- | --- | --- |
| Android phone | 720×1280 | 320 dpi | rounded | — | touch, keyboard, buttons |
| Android tablet | 2560×1600 | 320 dpi | rounded | — | touch, keyboard, buttons |
| Android foldable (the Pixel Fold) | 2208×1840 (folded 1080×2092) | 420 dpi | rectangle | hinge at 180°, 0–180° | touch, keyboard, buttons, hinge |
| Android dual-screen (`--device surface_duo`) | 2784×1800 (two 1350×1800 screens, an 84 px hinge) | 401 dpi | rectangle | hinge at 180°, 0–360° | touch, keyboard, buttons, hinge |
| Wear OS watch | 454×454 | 320 dpi | round | — | touch, rotary input, buttons |
| Android TV | 1920×1080 | 320 dpi | rectangle | — | D-pad remote, keyboard |
| Android Automotive | 1024×768 | 160 dpi | rectangle | — | touch, head unit |
| Android XR headset | 2560×2558 (landscape) | 240 dpi | rectangle | — | eye tracking, hand tracking, keyboard |
| Android XR glasses (`ai-glasses` images) | 450×450 (landscape) | 160 dpi | rectangle | — | eye tracking, hand tracking, keyboard |
| iPhone | 1206×2622 | @3x | rounded | — | touch, keyboard, buttons |
| iPad | 1668×2420 | @2x | rounded | — | touch, keyboard, buttons |
| iPhone Duo (iOS foldable) | 2853×2007 unfolded (its inner screen as it lies open; the framebuffer holds it 2007×2853), folded 1398×2034 | @3x | rounded | closed at boot, 0–180° | touch, keyboard, buttons, hinge |
| Apple Watch | 416×496 | @2x | rounded | — | touch, Digital Crown, buttons |
| Apple TV | 1920×1080 | @1x | rectangle | — | Siri Remote, keyboard |
| Apple Vision Pro | 2732×2048 | @2x | rectangle | — | gaze, pinch, keyboard |
| CarPlay | 800×480 | @2x | rectangle | — | touch, head unit |

The Android phone keeps simee's deliberately small default: smaller screens boot markedly faster.
New Wear, TV, Automotive and XR AVDs follow the SDK's own device definitions (`wearos_large_round`,
`tv_1080p`, `automotive_1024p_landscape`, `xr_headset_device`, `ai_glasses_device`, from
`sdklib/.../devices/*.xml` in `platform/tools/base`). They get the `config.ini` keys avdmanager
writes for those devices: `hw.device.name`, the sensors (`hw.accelerometer`, `hw.gyroscope`,
`hw.sensors.*`, with orientation following the gyroscope), `hw.mainKeys`, `hw.dPad`,
`hw.trackBall`, `hw.gps`, `hw.battery`, TV's `hw.screen=no-touch` and the glasses'
`hw.touchpad0`. The sensors decide what the guest's sensors HAL is offered: a TV has no motion
sensors, and an Automotive head unit has only light (plus the uncalibrated variants, which default
on as in stock). Automotive gets the definition's 3686 MB of RAM. `emu -create-avd` and
`-provision -create-avd` shape AVDs from the image the same way `simee create` does.
The Android XR profiles are the SDK's own device definitions (`xr_headset_device`, 4 GiB;
`ai_glasses_device`, 2 GiB; both landscape), and a new XR AVD gets their RAM and
`hw.initialOrientation=landscape` too.
Foldable postures follow the stock emulator's hinge ranges: closed below 30°, half-opened below
150°, opened from 150° to 180°. A dual-screen adds tent (210–330°) and flipped (330–360°).

## How a device's form factor is found

- **iOS**: from the device type identifier (`iPhone-Duo` is a foldable; `iPhone-…`, `iPad-…`,
  `Apple-Watch-…`, `Apple-TV-…`, `Apple-Vision-…`), else the runtime (`watchOS-…`, `tvOS-…`, `xrOS-…`). An unrecognized type has
  no form factor and is labelled by platform only. CarPlay is never inferred: it isn't a device
  type.
- **Android**: from the AVD's `config.ini` and image. The image tag decides XR (`android-xr*`,
  `google-xr`, `ai-glasses`), watch (`android-wear`, `android-wear-signed`, `android-wear-cn`), TV
  (`android-tv`, `google-tv`, and 16 KB page-size variants such as `google-tv-ps16k`) and
  automotive (`android-automotive*`); a hinge sensor
  (`hw.sensor.hinge=yes`) or a `fold` device name makes a foldable; a smallest width of 600 dp or
  more (Android's own threshold) makes a tablet; otherwise a phone. A Wear device name with
  `square` or `rect` gets a rectangular screen, else round.

## Launching each form factor

| Kind | Create | Drive |
| --- | --- | --- |
| iPhone | `simee create P "iPhone 17" "iOS 26.5"` | touch, `button home\|lock\|side\|action`, `type`, `rotate [--orientation …]` |
| iPad | `simee create T "iPad Pro 11-inch (M4)" "iOS 26.5" --form-factor tablet` | as iPhone, plus `swipe --edge bottom\|top`, `pointer X Y [click\|down\|up]` or `pointer X Y --to X2 Y2`, and `stylus X Y [tap\|down\|move\|up]` or `stylus X Y --to X2 Y2` (the Apple Pencil) |
| iPhone Duo | `simee create D "iPhone Duo" "iOS 27.1" --form-factor foldable` | as iPhone, plus `fold <fold\|unfold\|half-opened\|angle>` and `--display outer\|inner` on `screen`, `screenshot`, `tap`, `swipe` and `touch` ([iPhone Duo](ios-versions.md#iphone-duo)) |
| CarPlay | an iPhone on an iOS 26 runtime, booted: `simee carplay P on` | `tap\|swipe\|screenshot … --display carplay`; `carplay P off` |
| Apple Watch | `simee create W "Apple Watch Series 10 (46mm)" "watchOS 26.0" --form-factor watch` | touch, `rotary <ticks> [--delta <crown units>]`, `button home` (the crown's press) `\|side\|action`, `watch-gesture double-tap\|wrist-flick` |
| Apple TV | `simee create TV "Apple TV 4K (3rd generation)" "tvOS 26.0" --form-factor tv` | `button select\|menu\|back\|home\|play-pause\|up\|down\|left\|right` (the Siri Remote), `remote-swipe up\|down\|left\|right` (its touch surface) |
| Vision Pro | `simee create V "Apple Vision Pro" "visionOS 26.0" --form-factor vision` | touch (unverified), `button home` |
| Android phone | `simee create P --platform android --system-image "system-images;android-36;google_apis;arm64-v8a"` | touch, `button back\|home\|recents`, `type`, `rotate` |
| Android tablet | the same image, `--form-factor tablet` | as phone, plus `stylus X Y [hover\|down\|move\|up\|leave]` or `stylus X Y --to X2 Y2` (a pen; [Tablets](#tablets)) |
| Android foldable | the same image, `--form-factor foldable` (a Pixel Fold) or `--device surface_duo` (a dual-screen) | as phone, plus `fold <fold\|unfold\|closed\|half-opened\|opened\|flipped\|tent\|angle>`; console `fold`, `unfold`, `posture <id>`, `fold status` |
| Wear OS | `--system-image "system-images;android-34;android-wear;arm64-v8a"` | touch, `rotary <ticks>`, `button back\|home\|power` |
| Android TV | `--system-image "system-images;android-34;google-tv;arm64-v8a"` | `button up\|down\|left\|right\|select\|back\|home` |
| Automotive | `--system-image "system-images;android-34-ext9;android-automotive;arm64-v8a"` (API 34 and 35 Automotive images exist only at extension levels; `emu -provision 34 -tag android-automotive` picks `34-ext9`) | touch, `button back\|home`, `vehicle <property> [<value>]` (gear, speed, ignition, ...; unverified, see below) |
| Android XR headset | `--system-image "system-images;android-34;google-xr;arm64-v8a"` (install: `emu -provision 34 -tag google-xr`) | `xr recenter`, `xr rotate X Y Z` (radians), `xr move DX DY DZ` (metres), `xr options day\|night PASSTHROUGH`, `xr input mouse\|hand\|eye`, `xr viewport pan\|zoom\|rotate`, `xr pointer X Y [move\|down\|up\|click]`, `xr hand\|eye X Y [aim\|press]`, `xr gesture right-pinch\|…`, `type` |
| Android XR glasses | `--system-image "system-images;android-36;ai-glasses;arm64-v8a"` | `touchpad X Y [down\|move\|up\|tap]` or `touchpad X Y --to X2 Y2` (the temple touchpad, in its 1542×297 units), the `xr` controls but `xr pointer` (no dual-mode mouse); unverified, see below |

The device type and runtime names above are examples: `simee device-types`, `simee runtimes` and
`emu -list-images -tag <tag>` list what your host has (`-tag android-xr` lists the XR headsets by
the tag they carry; `-provision` takes the tag in the package path, `google-xr`).

`simee xr DEVICE ...` (the API's `xr` method) goes through simee's `xr` console command, which
sends the guest what the stock emulator's XR controls send over the `qemud:xr_service` pipe
(stock only offers them over gRPC: `xr hand` and `xr eye` send what its `streamInputEvent`
`xr_hand_event` and `xr_eye_event` forward, message types 12 and 13), and `xr pointer` drives the virtio dual-mode mouse simee adds
to XR images. `adb emu xr status` says whether the guest's XR service is connected and what it
last answered. The desktop gives XR devices look-left, recenter and look-right buttons.

`--form-factor` on iOS only checks the device type; on Android it also shapes the new AVD (screen
size, a foldable's hinge sensor). An image can't boot another family's form factor:

```text
$ simee create T --platform android --system-image "system-images;android-34;android-wear;arm64-v8a" --form-factor tablet
Error: system image `system-images;android-34;android-wear;arm64-v8a` (tag `android-wear`) boots Wear OS · Watch devices, not tablet
```

## Foldables

On iOS the foldable is the iPhone Duo: two built-in screens, the outer one in use folded and the
inner one unfolded, with no hinge sensor; [ios-versions.md](ios-versions.md#iphone-duo) has how
simee folds it. The rest of this section is Android's.

A new foldable AVD follows a device definition, with the `config.ini` keys avdmanager writes for
it: `--form-factor foldable` is the SDK's Pixel Fold (`pixel_fold`: `hw.sensor.hinge.*`,
`hw.sensor.posture_list`, the posture ranges, `hw.sensor.hinge.areas=1080-0-0-1840` and the
1080×2092 outer screen as `hw.displayRegion.0.1.*`, checked key for key against
`avdmanager create avd -d pixel_fold`), and `--device surface_duo` a dual-screen. The SDK has no
dual-screen definition; that one uses Microsoft's published Surface Duo sizes (two 1350×1800
screens either side of an 84 px hinge, 401 dpi), turns through 360° with stock's five postures,
and shows its left screen when flipped. Both work with the SDK's `emulator` too: the keys are
stock's own.

The console takes stock's `fold` (to the AVD's fold posture,
`hw.sensor.hinge.fold_to_displayRegion.0.1_at_posture`: a Pixel Fold shut, a dual-screen folded
back), `unfold` (flat) and `posture <id>` (1 closed, 2 half-open, 3 open, 4 flipped, 5 tent), with
stock's replies (`KO: Device is not foldable`, `KO: Posture 4 not supported`); `sensor set
hinge-angle0` moves the posture too, and `fold status` (simee) reports the angle, posture, whether
folded and the display size. `simee fold`, the API's `fold` and the desktop's Fold / Half open /
Unfold buttons go through them.

What the guest sees, as stock gives it:

- **Pixel Fold images** (`DeviceStateOnBoot`, `DeviceSkinOverlay`) get the device states the
  image ships for the AVD's device (`data/misc/pixel_fold/devicestate/`, sent at boot as
  `androidboot.qemu.device_state`), so the guest follows the hinge sensor itself: `dumpsys
  device_state` goes CLOSED, HALF_OPENED, OPENED with `fold`, `posture 2`, `unfold`. Once booted
  the device's skin overlays are enabled (`com.android.internal.emulation.pixel_fold`, as stock
  does), which give the framework the fold (`config_display_features` `fold-[1104,0,1104,1848]`)
  and the posture of each device state, what Jetpack WindowManager's `FoldingFeature` is built
  from. Folding shows Android's own "Swipe up to continue" over the app, as a real Pixel Fold does.
- **Other foldables** (the dual-screen) get the `device_posture` and `display_features`
  (`hinge-[1350,0,1434,1800]`) global settings on every posture change, as stock's
  `sendPostureToSystem` writes them for Jetpack WindowManager's emulator support.

Folded, the device shows its outer (or one) screen. Stock gives a Pixel Fold that screen as a
second built-in display; simee's board has one display, so a folded device's display is forced to
the folded screen's size (`wm size 1080x2092`, `1350x1800`), which the guest lays out for (an
app's `screenWidthDp` becomes the outer screen's, `adb exec-out screencap` is 1080×2092), and
readers of the scanout see only where the guest draws it: the display socket and `screenshot`
stream 949×1840 for the Pixel Fold (the 1080×2092 screen at the inner screen's height) and
1350×1800 for the dual-screen, and the desktop's view takes that shape. Touches on those frames
(the display socket's, the desktop's) land on the folded screen. The API's `screen`, `tap`,
`swipe` and `touch` are in the guest's layout instead (1080×2092, uiautomator's bounds): simee
scales their points onto the frames, so a tap at an element's bounds hits it; a screenshot stays
the 949×1840 frame. Unfolding resets the size, and so does stopping emu folded (`kill`, `adb emu
kill`, SIGINT) before the guest goes, so the AVD boots under the stock emulator at its own size; a
size emu couldn't reset (a crash) is recorded in `<avd>/simee/fold-wm-size` and reset at its next
simee boot. A `wm size` simee didn't set is left alone.

Checked on simee on 2026-11-26 (API 36 `google_apis` r7 image, Apple silicon host): a Pixel Fold
AVD and a dual-screen AVD each booted, folded, half-opened, unfolded and rotated; Settings opened
unfolded stayed in front through fold and unfold and was re-laid out for each screen; a tap on the
folded frame opened Settings search. The AutoMobile harness passed on both (all but the audio
tools, which reach a stock emulator's gRPC, and `deviceSnapshot`, which simee doesn't support),
and its observe, tap, swipe, launch and button tools passed with each folded. Later the same day:
folded, a `simee tap` at a Settings row's uiautomator bounds opened it, the harness passed folded
(40 of 41 tools, all but `deviceSnapshot`; audio tools not run), and after `adb emu kill` or
SIGINT folded the next boot's `wm size` had no override while a `wm size` set by hand survived a
boot; `simee fold` folded, opened and unfolded a Pixel Fold AVD run by the stock emulator 37.2.13
(its `dumpsys device_state` went CLOSED, OPENED).

## Tablets

What a tablet has under the stock tools and under simee. The Android column for stock is read from
the emulator's sources (`android-qemu2-glue/main.cpp`, `virtio-input-multi-touch.c`,
`multitouch-screen.c`) and the SDK's `pixel_tablet` definition; the touch screen's device codes were
also compared on a running stock emulator (phone, tablet and Wear; `docs/reference/stock-board.md`).

| Feature | Stock | simee |
| --- | --- | --- |
| Android screen, density | `pixel_tablet`: 2560×1600 (landscape is its natural orientation), xhdpi | the same 2560×1600 at 320 dpi (`--form-factor tablet`) |
| Android rotation | `rotate` turns the accelerometer's gravity; its window turns the screen to match, and its gRPC `getScreenshot` and `screenrecord screenshot` turn by the coarse orientation the accelerometer gives (`sensor set acceleration` alone turns them too). Checked on 37.2.13, a 1080×2400 phone: after 1 and 3 `rotate`s both are 2400×1080 and match `adb screencap`; after 2 they are the panel turned 180°, though the guest stays in landscape. Its gRPC touches stay in panel pixels | `screenshot`, the display stream (socket and frame ring) and the desktop show the screen turned as the guest rotated its display, which it logs as it rotates (not as the accelerometer says: with auto-rotate off, a portrait-locked app or a phone turned upside down the guest stays as it was, and so do simee's frames, matching `adb screencap` where stock's gRPC screenshots show the layout sideways), and `tap`, `swipe`, `stylus`, the API's `screen` and the console's `touch` and `pen` are in pixels of that upright screen (uiautomator's bounds), mapped onto the panel exactly in each orientation, a folded foldable's outer screen too. Phones, tablets and foldables only: TVs, cars, watches and XR never turn. Frames are turned once per frame only while turned and only for clients that don't turn them: the desktop asks for them as the panel lies and Rune turns them on the GPU as it draws ([android.md](android.md#performance-notes)) |
| Android stylus | every touch screen takes a pen on its own slot: `ABS_MT_TOOL_TYPE` pen, `MT_TOOL_MAX` with `BTN_TOOL_RUBBER` for the eraser, `BTN_STYLUS` for the barrel button, pressure; sent through gRPC (`PenEvent`). A pen at zero pressure gets no tracking id, so the guest sees no hover (checked with `dumpsys input` on 37.2.13, `docs/reference/stock-board.md`) | the same pen codes and pen slot on every touch screen, as stock has them on phones, tablets and Wear alike (`getevent -p` on both, 2026-11-26); `stylus` (API, CLI) and the console's `pen` send it; the desktop's Stylus button (tablets) makes the pointer a pen that hovers and draws. Checked on a Pixel Tablet-sized API 36 AVD: the guest lists the screen as `TOUCHSCREEN \| STYLUS`, a hovering pen reaches apps as `HOVER_ENTER`/`HOVER_MOVE`/`HOVER_EXIT` with tool type stylus (eraser: `ERASER`), a press with the button as `BUTTON_PRESS` with `BUTTON_STYLUS_PRIMARY`, and fingers stay `FINGER` after the pen; a finger held while the pen lifts stays touching, and so does a pen held while a finger lifts |
| Android mouse / trackpad | google_apis images enable neither `VirtioMouse` nor `VirtioTablet`, so stock gives a tablet no mouse device (a mouse device replaces the touch screen there); the host mouse is a finger | the same: the pointer is a finger, or the pen with the Stylus button |
| Android hardware keyboard | `hw.keyboard=yes` (`qwerty2`) | the same |
| Taskbar, split screen, freeform windows | the guest's own (large-screen launcher; split screen from the taskbar or recents; freeform behind the developer option) | the guest's own, unchanged. Checked on API 36: the 2560×1600 screen is `sw800dp xlrg land`; split screen (WMShell `splitscreen moveToSideStage`) put Settings and Clock side by side at 1270 and 1270 px; with `enable_freeform_support` a freeform Clock window opened at 776×1380 |
| Desktop window | — | a landscape tablet's screen is shown shorter so the window stays within 1440 points (other form factors keep the full-height screen) |
| iPad rotation, keyboard | Simulator.app's Rotate, hardware keyboard | `rotate [--orientation]`, `type`, `key`, `hardware_keyboard`. Checked on an iPad Pro 11-inch (M4), iOS 26.5: Safari turned to landscape and back, taps landed in the turned UI, and typing filled its address bar. Screenshots stay in the panel's portrait orientation, as `simctl io screenshot`'s do |
| iPad multitasking gestures | Simulator.app's edge swipes (home, Dock, App Switcher) | `swipe --edge bottom` (fast: home; slow or `--settle`: the app switcher), `--edge top` (Control Center, Notification Center); see [Apple inputs](#apple-inputs-and-carplay) |
| iPad pointer, Apple Pencil | Simulator.app can send the host pointer to the device (I/O › Input); it has no Pencil input | `pointer` (hover, click, drag, both buttons) and `stylus` (Pencil taps and strokes, no pressure, tilt or hover); see [Apple inputs](#apple-inputs-and-carplay) |

## Apple inputs and CarPlay

Each Apple form factor gets its own HID messages, the ones Simulator.app sends, through
SimulatorKit's HID client. Every message builder and selector is looked up first, so a runtime
without one fails that input with `Unavailable`, never a crash.

- **Apple TV**: `button` goes to the Siri Remote's own HID target (select `0x190`, menu `0x191`,
  TV `0x192`, play/pause `0x194`, volume, Siri), released with 0; the arrows are keyboard arrows,
  which the tvOS simulator maps to the remote's D-pad. `remote_swipe` sends a began → moved… →
  ended contact on the touch surface (the focus engine moves only for the whole sequence); the
  message's layout is checked before its phase fields are patched.
- **Apple Watch**: `rotary` turns the Digital Crown (`IndigoHIDMessageForDigitalCrownEvent`, split
  into steps of at most 200 units; one tick is 10 units, a calibration, or pass `delta`). Home is
  the crown's press; the side and Action (Ultra) buttons are hardware buttons. `watch_gesture`
  sends Double Tap or Wrist Flick (watchOS 11+, Wrist Flick 12+).
- **iPhone and iPad**: `rotate` sends the GSEvent orientation message Simulator.app's Rotate
  Left/Right send, to SpringBoard's `PurpleWorkspacePort`; without `orientation` it turns 90°
  clockwise from the last orientation simee set.
- **iPad**: `swipe --edge` (and `touch`'s `edge`) sets the edge flag Simulator.app sets on a
  drag that starts on the bezel (`IndigoHIDMessageForMouseNSEvent`'s `IndigoHIDEdge`: top 1, left
  2, bottom 3, right 4; SpringBoard logs them as `UIRectEdge` 1, 2, 4, 8). Without it a swipe from
  the edge pixel is an ordinary touch: it scrolls or draws in the app and never goes home. From the
  bottom a fast swipe goes home and a slow one (or `--settle`) opens the app switcher; from the top
  left Notification Center, top right Control Center, top middle the iPadOS 26 menu bar. iPhones
  take the same flag. In the desktop window a drag that starts in the 24 pt band just outside the
  screen (the bezel) is sent the same way, through the API's `touch` with `edge`, so home, the app
  switcher, Notification Center and Control Center open from the window as in Simulator.app;
  on an Android phone, tablet, foldable or watch it is an ordinary touch that starts on the edge
  pixel (gesture navigation's back and home, a watch's dismiss), since Android's edge gestures have
  no flag. TVs, Vision Pro, XR headsets and cars have no bezel. `pointer` creates the guest's mouse service
  (`IndigoHIDMessageToCreateMouseService`, whose message names the service's target) and sends it
  relative motion and button events (event types `0x11` and `0x7a41`, the layout SimulatorKit's
  `IndigoHIDMessageForPointerEventFromHIDEventRef` writes, built by `sim_core::indigo`); iPadOS
  then shows its pointer (in screenshots too), hover effects and Trackpad & Mouse settings. Motion
  is only relative, so the first call homes the pointer against the left and bottom edges (never the top,
  where iPadOS opens Notification or Control Center, nor a corner, which the rounded screen stops
  short) and moves in steps that slow to a stop, halving until the last report is at most 0.08 pt
  (an overrun of 0.04 pt); 10 pt steps with a last report of 0.3 pt were used until #122. iOS 27.2 carries
  the pointer on past a run of reports by half its last report (10 pt steps landed 5–11 pt long,
  median 7.9 pt over 12 targets), and pushes into an edge carry on into the next move; neither
  `HIDMouseAcceleration` (1.0 on both) nor backboardd's pointer acceleration preference changes
  that. Ending each run slowing down and pausing after the push into the bottom edge makes it
  exact on both (median and maximum error 0.0 pt over the same 12 targets on 26.5 and 27.2 with ten
  halving steps, read back by the probe app, 2026-11-26). With the six steps simee uses, 26.5 is
  still exact (0.0 / 0.0 pt) and 27.2 lands within a third of a point (median 0.32 pt, maximum
  0.36 pt over the 12 targets: about 0.1-0.2 pt long in x and 0.2-0.3 pt short in y). Each
  report is 16 ms. A server (`simee serve`) keeps where each call left the pointer, per boot, and
  the next call moves on from there in steps of up to 120 pt (which iPadOS also takes one to one),
  so a later click is a few reports instead of homing again (64 reports and a 400 ms settle for
  the Dock). It homes again after a failed call, a rotation through the API, a call whose reports
  went out more than 16 ms late (a stalled host), a call that ended on an edge or in a corner's
  40 pt square (where the guest may stop the pointer short), and every 100 calls.
  Touches (finger and Pencil, including the desktop window's, which sends no pointer events),
  idle time and relaunching the app don't move the pointer, and with the 0.08 pt last report its
  error doesn't add up: 120 calls to random points after one homing all landed within 0.32 pt on
  iOS 27.2, where 0.3 pt last reports had drifted to 0.78 pt (2026-11-27). Pointer input simee
  doesn't send (Simulator.app's pointer capture, another `simee` process or server driving the
  same device) moves the pointer without the server knowing until it next homes; so does a
  rotation from the desktop window, which turns the device without the API. Timings are in
  `benchmarks.md`. `pointer --to` drags in one call; a button held by `pointer … down`
  keeps the position for the next calls on the same server (`simee serve`) in the same boot. A
  `down` that runs in its own process (no server) is refused, since nothing would release it, as
  are an `up` with nothing held and a `move` or `up` after a failed call lost the held button
  (`click`, `down` or a drag first lets go of both buttons and homes). `stylus` on an iPad sends a touch whose finger is a stylus (the guest's `SimulatorHID` copies
  the transducer type into the `IOHIDEvent`), which apps see as `UITouch.type == .pencil` and
  draw with. Checked on iPad Pro 13-inch (M5) clones on iOS 26.5 and 27.2 with a probe app that
  logs every touch and hover, and with the AutoMobile harness.
- **CarPlay**: `carplay on` writes the device's StarkConfig (`extended_display.plist`: 800×480 @2x,
  touch), sets the iPhone's TVOut screen to that mode with CarPlay properties (found through the
  framebuffer server's screen adapter; an already-on plain display is turned off first, or the
  guest keeps it as AirPlay), and creates the CarPlay HID service. Touches then go to HID target 1
  and screenshots come from the external framebuffer (`display: "carplay"`). Only iPhones on iOS
  26 runtimes and earlier: iOS 27 renders the CarPlay screen black (Apple: "iOS Simulator is not
  supported for CarPlay"). `create --form-factor auto` on iOS says to do this instead, since
  CarPlay isn't a device type.

## Checked against real images

Each form factor's current SDK image was provisioned with `emu -provision`, given an AVD by
`emu -create-avd`, and compared offline with simee's board: `emu -avd NAME -dry-run` for what
simee would give it, `scripts/inspect-image.py` for what the image expects (its features, the
first-stage fstab and modules in every ramdisk archive, and the vendor partition's input
configuration, keylayouts and HALs). None of this boots the image.

| Image | What matched | What changed in simee |
| --- | --- | --- |
| `android-wear-signed` 36 r1 | fstab slots, modules, `.idc` names (the rotary is a `rotaryEncoder`), every `button` key in `qwerty2.kl` | `-provision` found it (it lives on the `android-wear` site) |
| `android-tv` 34 r3 | fstab slots, modules, `.idc` names, every `button` key in `qwerty.kl`; generic goldfish vendor, no TV-only HAL | no touch device (`hw.screen=no-touch`), no motion sensors; `system.img` 8.1 → 1.4 GB on disk |
| `android-automotive` 34-ext9 r5 | fstab slots, modules; `build.prop` beside the image (car detection); the VHAL opens `pipe:qemud:car`; every `vhal` property and gear/ignition value | `-provision 34` resolves to `34-ext9`; light-only sensors, 3686 MB |
| `google-xr` 34 r7 | fstab slots, modules, `.idc` names; `GuestAngle` and `VirtioDualModeMouse` | the dual-mode mouse follows `VirtioDualModeMouse` |
| `ai-glasses` 36 r6 | fstab slots, modules, `kernel_cmdline.txt` | the touchpad (`virtio_input_multi_touch_touchpad_0`), no dual-mode mouse |

Every fstab mounts system through `androidboot.boot_devices` (slot 27, `a003600`), userdata as
`vdc` and metadata at `a003c00`; every first-stage module list is virtio blk, console, mmio, rng
and vsock (plus PCI, unused). All five enable `VirtioSndCard`; Wear, TV and XR run the goldfish
audio HAL that phones boot without a card on.

## Known gaps

What is modelled and labelled but not (or not verifiably) working end to end:

| Kind | Gap |
| --- | --- |
| CarPlay | Implemented as above but never run: no Mac was available. Whether `simctl io enumerate` then labels the screen "CarPlay" (what auto-mobile's CarPlay observe looks for, auto-mobile #18729) is unconfirmed. Only touch: the knob, back, home and Siri head-unit inputs aren't sent (auto-mobile #19059 is finding out whether SimulatorKit takes them). The desktop doesn't show the CarPlay screen. |
| Android Automotive | simee serves the vehicle HAL channel (`qemud:car`, driven by `vehicle` / console `car`) and the virtio-snd card the car audio HAL needs, but no car image has been booted on them yet (checked offline against 34-ext9: [android.md](android.md#automotive)); EVS, the cluster display and the rotary controller aren't modelled ([android.md](android.md#automotive)). |
| Android TV | `android-tv` 36 boots on simee with the virtio-snd card, as stock gives it (its ranchu audio HAL plays the UI sounds through ALSA card 0, like the phone images; [android.md](android.md#automotive)); the rest of this row is unchecked on a running TV image. Checked offline against `android-tv` 34 r3 (`emu -avd TV34 -dry-run` plus its unpacked ramdisk and vendor partition): the first-stage fstab addresses the board's slots (system at `a003600`, userdata `vdc`, metadata at `a003c00`), its first-stage modules are simee's devices, the vendor partition is the generic goldfish one (no HDMI-CEC or TV-input HAL), the `.idc` files match simee's input names, and every `button` key maps to its Android key in the `qwerty.kl` the guest uses. Its AVDs say `hw.screen=no-touch` like stock's, and simee then attaches no touch device, as stock doesn't; `tap`, `swipe` and console `touch` say so. |
| Android foldable | Folded, a Pixel Fold's outer screen is the inner display forced to 1080×2092 and drawn at 949×1840 on the host (stock: a second 1080×2092 display, as on the device), so host frames of it are scaled by 0.88 and the image's own two-display layout (`display_layout_configuration.xml`, `REAR_DISPLAY_MODE`) isn't used. A dual-screen gets no device states (stock gives other foldables a lid-switch state machine and `wm folded-area` with a lid event; simee forces the size instead), and nothing masks its hinge. Jetpack WindowManager's `FoldingFeature` is configured as stock configures it (the overlays and settings above) but no app has read it on simee yet. The stock emulator itself wasn't run beside it: its behaviour is from its binary's strings, the image's files and avdmanager's output. |
| Android XR | Not booted on simee yet. Checked offline against `google-xr` 34 r7: its fstab, modules and input `.idc` names match the board, and its features ask for what simee gives it (`GuestAngle`, `VirtioDualModeMouse`). It enables `VirtioSndCard`, so simee gives it the virtio-snd card as stock does; its ranchu audio HAL, the phones' one, plays through it (booted on phones, Wear and TV, not yet on XR). The room renders in the guest through gfxstream (simee already defaults to ANGLE), but stock's other XR feature flags (`XrModeUI`, `XrHandAndEyePointers`, `VirtioMouse`, `MultiDisplay`, `BluetoothEmulation`) have no simee equivalent yet. The dual-mode mouse matches stock's device (name, ids, event bits, axes); how the guest maps it to its hand ray and gaze pointers, and whether head pose, options, hand and eye events over `xr_service` take effect, is unverified. auto-mobile's `xrHeadsetInput` (auto-mobile #19076) sends hand and eye events over stock's gRPC `streamInputEvent`, which simee doesn't serve yet (its gRPC `EmulatorController` is on the roadmap); `xr hand`/`xr eye` put the same messages on the guest's pipe. The display glasses' temple touchpad (`hw.touchpad0`, 1542×297, which the `ai-glasses` image configures as `virtio_input_multi_touch_touchpad_0`) is attached and driven by `touchpad`, but how the guest's UI reacts to it is unverified. Only images that enable `VirtioDualModeMouse` (the headsets, not the glasses) get the dual-mode mouse, as on stock. |
| Apple Watch, Apple TV | The crown, gestures, Siri Remote buttons and touch surface use the messages Simulator.app sends (confirmed live by other projects on watchOS/tvOS 26.5 and 27), but not run by simee. The crown's units per tick are a calibration. From Xcode 27 the guest can switch input to `dtuhidd` (once `com.apple.coredevice.dtuhidd.active` is set, e.g. by Device Hub), after which legacy HID buttons and keys are dropped; simee doesn't detect that yet. |
| Vision Pro | Gaze, pinch and the camera (look around, pan, orbit) go through Xcode's visionOS UI plugin in Simulator.app, whose messages aren't public; simee sends plain touches, which no source confirms visionOS honours. auto-mobile can't script visionOS input either (auto-mobile #19055). |
| iPad stylus (Apple Pencil) | Pressure, tilt, hover and the barrel (double-tap, squeeze) aren't reachable: the guest's `SimulatorHID` makes every touch with `IOHIDEventCreateDigitizerFingerEvent` and copies only position, the transducer type, radii and quality fields, so altitude and azimuth stay π/2 and 0; `UITouch.force` and `maximumPossibleForce` stay 0 because the screen's digitizer has no force (the iPad device types say `SupportsForceTouch` false; a pencil digitizer service, which backboardd looks for, isn't one Indigo can create), and Simulator.app's own pressure message (`IndigoHIDMessageForPressureEvent`) changes nothing on an iPad either. A contact in range but not touching makes no hover. Simulator.app has no Pencil input at all; the barrel gestures come from the Pencil's own accessory, which no HID message models. The simulator runtimes ship no Notes app, so drawing was checked in a probe app. |
| iPad pointer | Positions are open loop (the guest takes relative motion only): exact on iOS 26.5 and 27.2, but a future release that moves the pointer differently again would put them off without simee noticing (simee doesn't read back where the pointer is). Pointer coordinates assume portrait. No scroll wheel or trackpad gestures yet (the guest also takes scroll and dock-swipe events). |
| Android rotation | API 29 and 30 aren't followed: their logcat filter goes through `qemu.cmdline` (91 bytes), which emu leaves as the user's, so a rotated guest's frames, `screen` and taps stay as the panel lies while `adb screencap` and uiautomator turn, and the console's `rotation` says `unknown`. On API 31 and later the turn is read from the guest's log, a few milliseconds after the guest relayouts: a tap sent in that window maps with the old turn (read `rotation` until it changes). On API 31 under simee the guest has no touch screen at all (its 5.10 kernel fails to probe `virtio_input_multi_touch_1`, error -12), so taps there were not checked. |
| Android tablet pointer | No mouse or trackpad is attached (stock attaches none to google_apis tablets either); hover and right-click aren't available except as the stylus's hover and barrel button. |
| iOS rotation | Runtimes that report device motion can ignore the orientation event (idb switches to a vendor-defined HID message through `dtuhidd` there); not handled. |
| Every Apple form factor | Needs a Mac with the runtime installed; none could be booted in the environment the combined project was built in. |
