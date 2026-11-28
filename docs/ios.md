# iOS backend notes

How simee's iOS backend reaches parts of a simulator that the stock tools don't expose, with the
measurements behind each design choice. `docs/api.md` is the reference for the methods themselves.

## App preferences

`prefs_*` on an iOS device (`crates/sim-api/src/prefs/ios.rs`) reads an app's user defaults from
the simulator's files on the host, writes through the device's cfprefsd, and follows changes
through an in-device observer of cfprefsd, with the files as the fallback.

**Stores.** An app's own domain is `Library/Preferences/<bundle id>.plist` in its data container
(`data/Containers/Data/Application/<uuid>`). An app-group suite is in the group's container
(`data/Containers/Shared/AppGroup/<uuid>/Library/Preferences/<group>.plist`) when the app is
entitled to the group, and in the app's own container otherwise: that is where cfprefsd keeps a
suite the app isn't entitled to. A simulator app's entitlements are the ones linked into its
executable's `__TEXT,__entitlements` section (Xcode's "simulated entitlements"); a group listed only
in the code signature gets a container from installd but cfprefsd doesn't treat it as the app's
group. simee reads `com.apple.security.application-groups` from both places. Any other `.plist` in
the app's `Library/Preferences` is listed as a named domain. Which container belongs to which app or
group comes from each container's `.com.apple.mobile_container_manager.metadata.plist`, decoded in
process (`prefs/plist.rs`, a binary plist reader and writer) and cached; a reinstalled app gets a
new container, which is found again on the next call.

**Reads** decode the binary plist on the host: no spawn, no service call.

| Read (2026-11-27, iOS 27.2, iPhone 17 Pro, M3 Max host, `simee serve`, round trip over the socket) | Median |
| --- | --- |
| `prefs_read` of one store | 0.11 ms (n=20) |
| `prefs_read` of every store (3) | 0.27 ms (n=10) |
| `prefs_stores`, warm | 0.13 ms (n=10) |
| `prefs_stores`, first call in a process (scans every container's metadata, reads the executable's entitlements) | 133 ms (n=1) |
| `prefs_read {fresh: true}` (one in-device spawn) | 182 ms (n=5) |

cfprefsd holds a write in memory and writes the file later, so a plain read can be behind the app.
With an app writing every 3 s, cfprefsd wrote the file about every 10 s: a write reached the file
0.5 to 9.5 s after it was made (from the newest write a flush covered: median 1.5 s; from the
oldest: median 8.5 s; n=6). `fresh: true` asks cfprefsd instead, which always has the latest values.

**Writes** run `crates/sim-api/native/prefstool.m` in the device (built on first use with the
simulator SDK and cached, as `sim-netshape`'s `scproxy` is), one spawn per batch:
`_CFPreferencesSetAppValueWithContainer` for each key, then one
`_CFPreferencesAppSynchronizeWithContainer`, with the store's container, so cfprefsd writes the
file the app reads. The public `CFPreferencesSetAppValue` from a process outside the container
writes `data/Library/Preferences/<domain>.plist` instead, a file the app never reads (while still
firing the app's KVO observers, with a value its `UserDefaults` doesn't have). The container calls
are SPI, looked up with `dlsym`: a runtime without them gets an error, not a write to the wrong
file. A write takes 180–187 ms (one key or ten); the device must be booted.

- A running app sees a write at once: with the app running, every type of a ten-key batch
  (string, int, long, float, double, bool, date, data, array, dictionary) was in its
  `UserDefaults` 1.7 ms after the call returned, and its KVO observers fired for each key. A
  write to its app-group suite was seen the same way. So a write answers `visible: now`,
  `app: untouched`.
- `UserDefaults.didChangeNotification` does **not** fire in the app for a write from another
  process; it fires only for the app's own writes. Observe a key with KVO to react to a write
  made by simee.
- Every op is checked (types, the store's container, the request) before the helper sends the
  first key, so a refused batch changes nothing. cfprefsd then takes the keys one at a time: for a
  moment a reader can see the first keys of a batch before the last, and a file cfprefsd creates
  for a new domain can first be written with only the first key.
- iOS keeps no width: `int` and `long` write a plist integer, `float` a 4-byte real (the app reads
  `0.1` as a `Float`), `double` an 8-byte one; every read gives `long` and `double`
  (`0.10000000149011612` for that float). A date keeps about a microsecond (a plist date is a
  double of seconds since 2001).

**The feed** has two sources: an in-device observer, which hears about each change from cfprefsd
as it is made, and the store files, which cfprefsd writes about every 10 s.

- **The observer** (`crates/sim-api/src/prefs/ios/prefswatch.m`, built on first use and cached
  like the write helper) runs in the device as its own process, started with the feed. It is not
  injected into the app: no `DYLD_INSERT_LIBRARIES`, no relaunch, and it works for an app that is
  already running. It opens each store's domain in the container cfprefsd keeps it in
  (`-[NSUserDefaults _initWithSuiteName:container:]`) and observes one key no app uses with KVO.
  That makes cfprefsd send the process every other process's changes to the domain. cfprefsd sends
  them for every key, observed or not, to
  `-[CFPrefsSearchListSource handleChangeNotificationForDomainIdentifier:isRemote:]`, which the
  observer wraps in its own process. On each notice it reads the domain
  (`_CFPreferencesCopyMultipleWithContainer`) and prints its values if they changed. The host diffs
  them against what it last had and reports each changed key. `NSUserDefaultsDidChangeNotification`
  doesn't fire for another process's write, and a KVO observer fires only for the keys it names,
  so neither can be the signal on its own.
- Its output is a pipe to the host (`Device::exec_stream`), with no socket. Each line is `snap <i>
  <base64 binary plist>` for store `i` (its first look at each store, then each change), then
  `ready`, or `error <why>`. A store whose values take more than 256 KiB as a binary plist isn't
  sent: the observer prints `large <i>` once instead, and the feed follows that store's file (with
  one line on stderr) until a `snap` of it comes again. The feed reads at most 1 MiB of a line. While
  it waits the observer sits in the run loop with no timer. The feed signals it when the feed ends,
  the device shuts down, or its output can't be followed (a line that isn't one of these, or too
  long); a helper that has exited (its output closed) is never signalled. It also exits by itself
  when the host process exits (a process-exit source on the host's pid), or at its next line once
  its output is closed. Each wrapped class's handler keeps that class's own original, so a
  subclass's handler calling `super` runs each original once.
- **Fallback.** While the observer follows a store, the store's file is not read: the file is
  behind cfprefsd, and reading it would report a newer value changing back. If the observer can't
  be built or started, its runtime lacks one of the SPI (it checks each with `dlsym` or
  `respondsToSelector:`, and the wrapped method's signature, then prints `error`), or it exits
  (before or after `ready`), the feed follows the files as before. It starts again from each file as it is then, and prints
  one line on stderr saying why. A domain whose file first appears after the feed started is
  followed only by its file.

The files are watched with kqueue (`sim_coresim::FileWatch`) throughout. cfprefsd replaces a plist
by renaming a new file over it, which its directory reports. On each wake simee re-reads the files
whose inode, size or modification time changed and reports each key whose decoded value changed. A
file caught half-written is read again on the next wake, and a new domain file in the app's
container is followed from empty. The watcher wakes at least every 500 ms to see whether its feed
has ended or the device has shut down (from the `state` in the device's `device.plist`). A feed on
a device that shuts down ends 3.5 s after `simctl shutdown` began (measured).

| Feed through the files (2026-11-27; same device and app; the app writes a counter every 3 s) | Median |
| --- | --- |
| cfprefsd writes the file to the feed's event | 1.0 ms (0.8–1.1, n=6) |
| The newest in-app write a flush covers to the event | 1.5 s (0.5–2.5 s, n=6) |
| The oldest in-app write a flush covers to the event | 8.5 s (7.5–9.5 s, n=6) |

The observer's mechanism was checked on iOS 27.2 (iPhone 17, 2026-11-28) with a prototype of it in
the device. The app wrote a counter every 3 s, and the prototype heard each write 1.4 to 2.1 ms after
the app logged it (n=4). Writes from another process were heard the same way: a new key, a deleted
key, and the app-group suite. The prototype exited with its host process. The feed's own latency
from an in-app write to the `prefs_watch` event has not been measured yet.

Reproduce: build a test app that shows its `UserDefaults` and logs its writes and what it sees with
wall-clock times, entitled to an app group through `-Xlinker -sectcreate -Xlinker __TEXT -Xlinker
__entitlements`; run `simee serve`, then time `prefs_*` calls over the socket and match `prefs_watch`
events against the app's log.

## Camera

A simulator has no camera: AVFoundation finds no video device, and nothing in CoreSimulator or
SimulatorKit carries frames ([research/audio-camera.md](research/audio-camera.md) §3). simee gives
apps one by injecting a capture shim (`shims/ios-camera`) and feeding it live from a camera source
(`crates/sim-camera`, [camera.md](camera.md)).

**Pieces.**

- The feed (`simee_camera::ios_feed::Feed`) serves one device's linked source on a Unix socket:
  `cam.sock` in the device's data directory, or a name in the host's temp directory when that path
  is over the 103-byte socket limit (`ios_feed::socket_path`, which creates and checks a private 0700 per-user directory there; the socket itself is 0600). Each reader says the size and rate it
  wants and is sent BGRA frames, each with its size, the source's frame number and the link's
  generation, as the source produces them. Readers of one source share one capture. Relinking
  (`Feed::set_source`) moves every reader to the new source without a reconnect.
- `libsimee-camera.dylib` is the library to insert. It links only libSystem and does nothing unless
  `SIMEE_CAMERA_SOCKET` is set and the process is a user-installed app (its executable is under
  `Containers/Bundle/Application`, not in an `.appex`). Then it loads `libsimee-camera-hooks.dylib`
  from beside itself, which checks the bundle id again and skips `com.apple.*`.
- The hooks give AVFoundation a back and a front camera ("simee Back Camera", "simee Front
  Camera"): discovery sessions, `defaultDeviceWithMediaType:`,
  `defaultDeviceWithDeviceType:mediaType:position:`, `deviceWithUniqueID:`, `AVCaptureDeviceInput`,
  `AVCaptureSession` (with the `running` KVO and the start and stop notifications), video data
  output sample buffers, preview layer content, QR codes through `AVCaptureMetadataOutput` (read from
  the live frames about twice a second) and `AVCapturePhotoOutput` stills of the latest frame. A
  process holds one connection to the feed while a session runs, and reconnects if the feed restarts.
- `CameraFixture.app` (`dev.simee.camerafixture`) shows the preview and reports the test card's frame
  number read back from each sample buffer's pixels, the frame rate and QR codes, on screen and in
  `Documents/camera-report.json`. `-position front` in its launch arguments opens the front camera.

`shims/ios-camera/build.sh [OUT_DIR]` builds all three for the simulator with `xcrun clang`.

**Through the API** (#323, [camera.md "iOS simulators"](camera.md#ios-simulators)):
`camera_link {device, facing, source}` (`simee camera link <udid> <source>`), `camera_unlink`,
`camera_links` and `boot`'s `camera_back` / `camera_front` do the device-wide steps below: the
simee server serves the device's feed on `cam.sock`, injects the shim found at
`$SIMEE_CAMERA_SHIM` or `target/ios-camera/` (the error names `build.sh` when it is missing),
saves the link in `<data>/simee/cameras.ini` and reapplies it when simee boots the device. A
booted device can be linked only through a running `simee serve`, which holds the feed. Running
apps need a relaunch and an app needs its camera grant, as below. The device inserts a copy of
the shim kept in `<data>/simee/camera-<hash>/`, and the insertion stays for the rest of the boot
if the server exits or the device is shut down outside simee: `simee camera unlink <udid>` or a
reboot clears it.

**Per app**: grant the camera the usual way, then launch through the API with the shim and the
socket in the environment (simee passes `env` straight to the launch, no `SIMCTL_CHILD_` prefix):

```bash
simee privacy <udid> grant camera <bundle id>
simee call launch '{"device":"<udid>","bundle_id":"<bundle id>","env":{"DYLD_INSERT_LIBRARIES":"<out>/libsimee-camera.dylib","SIMEE_CAMERA_SOCKET":"<data dir>/cam.sock"}}'
```

**Device-wide**: set both variables in the device's launchd and relaunch the apps that should see
the camera; apps started afterwards, from the home screen too, get it. Join an existing
`DYLD_INSERT_LIBRARIES` (the network shaper's) with `:` rather than replacing it, and unset both
when the device is unlinked:

```bash
simee exec <udid> launchctl setenv DYLD_INSERT_LIBRARIES <out>/libsimee-camera.dylib
simee exec <udid> launchctl setenv SIMEE_CAMERA_SOCKET <data dir>/cam.sock
simee exec <udid> launchctl unsetenv DYLD_INSERT_LIBRARIES   # and SIMEE_CAMERA_SOCKET, to unlink
```

Measured 2026-11-27 on an iPhone 17 simulator (iOS 26.5), `pattern:testcard` at 720x960@30 from
`cargo run -p simee-camera --example ios-feed`: the fixture received 30-31 fps on the back and the
front camera with the decoded frame number advancing, both when launched through `launch` with `env`
and when tapped on the home screen after `launchctl setenv`. Relinking to `qr:hello` reached the
app's sample buffers in 0.17-0.27 s and its metadata output in 0.43-0.51 s (n=2). Settings, launched
after the device-wide `setenv`, did not load the hooks.

**Limits.**

- Apple's apps and daemons (`com.apple.*`, and anything in the runtime root) are never injected:
  Camera, and system camera UI such as `UIImagePickerController`'s camera and VisionKit's
  `DataScannerViewController`, stay without a camera.
- ExtensionKit and app extension processes (WebKit's among them) get a sanitized environment or are
  skipped, so `getUserMedia` in a web view and app extensions have no camera.
- Security: the socket is mode 0600, but any process running as the same user (any app in any
  simulator on the host) can connect to a linked feed and read its frames, including a host webcam,
  without a camera permission of its own. The shim stops reading and disconnects as soon as no
  session runs, reconnects with a 250 ms to 5 s backoff, and the dylibs are universal (arm64 and
  x86_64, built with `lipo`; not run on an x86_64 simulator here).
- Already-running processes need a relaunch to pick up a device-wide `setenv`.
- `network_shape`'s `launchd` injection sets (and its clear unsets) `DYLD_INSERT_LIBRARIES` whole,
  so it drops a camera link's shim: link the camera again after shaping. A camera link keeps the
  shaper's library.
- Not yet verified on a device through the API (the feed, the injection and the boot reapply are
  tested with fakes). The check: build the shim and the fixture, `simee serve`, then on a
  simulator you created: `simee install <udid> target/ios-camera/CameraFixture.app`,
  `simee privacy <udid> grant camera dev.simee.camerafixture`, `simee camera link <udid>
  pattern:testcard`, `simee launch <udid> dev.simee.camerafixture`, read
  `Documents/camera-report.json` (frame numbers advancing), `simee camera link <udid> qr:hello`
  (the QR code reported without a relaunch), `simee shutdown <udid>`, `simee boot <udid>` (the
  link comes back: `simee camera links <udid>` says `linked`), `simee camera unlink <udid>`.
- Frames are BGRA, upright and unmirrored, at the size the app's connection asked for (the feed's
  default, or `SIMEE_CAMERA_SIZE=WxH@FPS`). Orientation, mirroring, zoom, focus, exposure and frame
  duration settings are accepted and ignored; there is no torch or flash, and `formats` is empty.
- While the shim is active every `AVCaptureSession` in the app is simulated: an audio input added to
  a session records nothing.
