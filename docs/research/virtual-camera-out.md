# A device's screen as a host camera: research

Spike for #326 (epic #319): can a booted simee device show up as a camera on the host, so Zoom,
Meet, OBS or a browser `getUserMedia` call can pick "simee: iPhone 17 Pro"? What it costs on macOS
(a CoreMediaIO camera extension) and Linux (v4l2loopback), and what works today with no new code.

- Host: macOS 26.6.2 (25G83), Xcode 26.6, Apple silicon. The Linux parts were read from source and
  docs, not run (no `/dev/video*` here).
- The probe is `scratch/vcam-out/cmiolist.c` (untracked; its header gives the build command). It
  lists CoreMediaIO devices and each stream's direction from an ad-hoc signed command-line binary.
- Each finding is tagged **[verified]** (seen on this host, in a header, or in the cited source
  code) or **[inferred]** (from docs, forum reports or reasoning, not run here).

## Summary

| Question | Answer |
| --- | --- |
| Can simee's current distribution ship a camera extension? | No. A camera extension is a system extension inside a `.app` that must run from `/Applications`, signed with a team that has the System Extension capability (Developer ID, notarized for users). simee ships `cargo install` binaries and release tarballs, ad-hoc signed. **[verified]** |
| What would? | A Developer ID signed, notarized `Simee.app` (Homebrew cask or `.dmg`) that holds the extension. The crates.io CLI can then feed it frames. **[inferred]** |
| User approval | Once per extension, in System Settings > General > Login Items & Extensions > Camera Extensions (macOS 15+). Version updates go through a replace request; no new approval is documented, but some updates complete only after a reboot. **[verified path; inferred update story]** |
| Frames from another process | Yes. The extension exposes a sink stream; any process finds it with the public CMIO hardware API and enqueues `CMSampleBuffer`s wrapping IOSurface-backed pixel buffers. OBS does exactly this. **[verified device and sink visible from a CLI; inferred enqueue without the host app]** |
| Latency, frame to host app | Not measured. Expected about one to two frames (17 to 50 ms) from the sink queue and the extension's consume timer. **[inferred]** |
| Legacy DAL plug-in | Deprecated in 12.3, not loaded since macOS 14.1 unless the user sets a Recovery Mode override. Dead end. **[verified]** |
| Linux | v4l2loopback is an out-of-tree module (DKMS, root to load). Once a device exists, a user in `video` writes frames with `VIDIOC_S_FMT` + `write()`. No signing, small. **[verified from OBS and v4l2loopback source]** |
| Zero-effort fallback | OBS Virtual Camera with a macOS Screen Capture (window) source on the simee desktop window, or an OBS Browser Source on the WHEP player page once #266 ships. **[inferred]** |
| Recommendation | Document the OBS fallback now; build the Linux writer when #321/#327 need it; build the macOS extension later, after a signed `Simee.app` exists. |

## 1. macOS: a CoreMediaIO camera extension

### Requirements

| Requirement | Detail |
| --- | --- |
| macOS floor | `CMIOExtension*` is `API_AVAILABLE(macos(12.3))` in the SDK headers **[verified]**. OBS builds its extension for 13.0 and turns it on only on 13+ **[verified, `camera-extension/CMakeLists.txt`, `plugin-main.mm`]**. simee already needs macOS 15+ (README), so the floor costs nothing. |
| Packaging | The extension lives in the host app's `Contents/Library/SystemExtensions/`. Apple: "Only apps that reside in the `/Applications` directory can activate an extension." OBS fails with `OSSystemExtensionErrorUnsupportedParentBundleLocation` and asks the user to move OBS there. **[verified, Apple docs, OBS source]** |
| Entitlements | Host app: `com.apple.developer.system-extension.install` and an app group. Extension: `com.apple.security.app-sandbox` (required for camera extensions), the same app group, `CMIOExtension.CMIOExtensionMachServiceName` in its Info.plist, prefixed with the team ID. Seen on an installed third-party camera extension on this host. **[verified]** |
| Signing | Activation checks that "the entitlements in the code signature match the entitlements granted to your development team". Ad-hoc signing (what simee does now, `scripts/sign-run.sh`) cannot carry a team, so it cannot activate. **[verified, Apple docs]** The workaround, `systemextensionsctl developer on`, needs SIP disabled. **[inferred, forum reports]** |
| Notarization | For users outside the developer's machine: Developer ID Application signature, hardened runtime, notarized and stapled. The installed reference extension has `flags=0x10000(runtime)`, Developer ID authority and a stapled ticket. **[verified on the reference app; inferred as required]** |
| Process model | The extension runs as the system role user `_cmiodalassistants`, sandboxed. XPC from the logged-in user's processes is unreliable and DTS advises against it; the sink stream is the supported data path. **[inferred, Apple forums thread 706184]** |
| Membership | A paid Apple Developer Program team, and a Developer ID certificate plus notarytool credentials in the release workflow. `scripts/release.sh` has no Developer ID signing or notarization step today. **[verified]** |

### Distribution fit

- **crates.io / release tarball only: does not fit.** `cargo install` cannot produce a bundle in
  `/Applications` with a team-signed, notarized system extension. **[verified by the requirements]**
- **Homebrew cask or a `.dmg`: fits.** A cask installs an `.app` into `/Applications`, the way
  `brew install --cask obs` delivers OBS and its camera extension. The cask can also link the CLI
  (`binary` stanza). **[inferred]**
- **Split install works.** The extension and its activating app need the signed bundle; the frame
  producer does not. A crates.io `simee` CLI can find the camera and feed its sink once `Simee.app`
  has activated it (next section). So the cask is a one-time "camera support" install, not a
  replacement for `cargo install`. **[inferred; enumeration verified]**

### Approval and updates

1. `Simee.app` submits `OSSystemExtensionRequest.activationRequest(forExtension:)`. The delegate gets
   `requestNeedsUserApproval`, and the user enables the extension in System Settings > General >
   Login Items & Extensions > Camera Extensions on macOS 15+ (Privacy & Security > Security before).
   This host's `systemextensionsctl list` prints that path for the `com.apple.system_extension.cmio`
   category, and OBS's macOS 15 error string names it. **[verified]**
2. Updates: bump the bundle version and submit the same activation request. The manager calls
   `request(_:actionForReplacingExtension:withExtension:)` when the team and bundle IDs match and the
   versions differ; OBS always answers `.replace`. Apple documents no second approval for a
   replacement **[verified docs, OBS source; inferred: no re-approval]**. The result can be
   `willCompleteAfterReboot`, OBS handles it, and developers report most CMIO updates need a reboot.
   **[verified OBS code; inferred frequency, forum 702066]**
3. So keep the extension a thin, rarely versioned shim (source stream, sink stream, placeholder
   frame) whose version is decoupled from simee's. A simee release then never touches it.
   **[inferred]**

### Frames from another process: the sink stream

OBS's extension is the model **[verified, `obs-studio/plugins/mac-virtualcam`]**:

- The extension's device has two streams: a source (what Zoom reads) and a `.sink` stream
  (`CMIOExtensionStreamDirectionSink`, "consumes sample buffers for playback").
- The producer (OBS's plugin, inside OBS.app) lists `kCMIOHardwarePropertyDevices`, matches
  `kCMIODevicePropertyDeviceUID`, takes the second stream, gets its queue with
  `CMIOStreamCopyBufferQueue`, starts it with `CMIODeviceStartStream`, and for each frame fills a
  `CVPixelBufferPool` buffer created with `kCVPixelBufferIOSurfacePropertiesKey` (IOSurface-backed),
  wraps it with `CMSampleBufferCreateForImageBuffer`, and calls `CMSimpleQueueEnqueue`.
- The extension polls `consumeSampleBuffer(from:)` on a timer at 3× the frame rate, re-sends each
  buffer on the source stream with `send(_:discontinuity:hostTimeInNanoseconds:)`, and calls
  `notifyScheduledOutputChanged`. When nothing feeds the sink it sends a placeholder frame.

On this host, `cmiolist` (ad-hoc signed, not in an app bundle) listed an installed third-party
camera with two streams, `37(input/source) 38(output/sink)`, in 24 ms **[verified]**. Enumeration
needs no entitlement. Enqueueing from a process that is not the host app, and whether that triggers
a Camera privacy prompt for the terminal, were not tried (it would write into another vendor's
camera); the build issue checks both against simee's own extension first. **[inferred]**

**IOSurface sharing.** simee already has the producer half: `sim-encode`'s `IosSource` wraps the
device framebuffer IOSurface as a `CVPixelBuffer` with no copy (0.44 ms p50 on an iPhone 17 Pro,
`docs/streaming.md`), and `AndroidSource` copies emu's RGBA frames into an IOSurface-backed pool
**[verified]**. For a camera, the live iOS framebuffer should not be enqueued directly: the guest
keeps drawing into it while the extension and the app read it, so it tears. Copy (or GPU-blit) into
a pool buffer per frame. IOSurface-backed buffers cross to the extension as surface ports, not pixel
copies. **[inferred]**

**Shape.** Conferencing apps expect landscape 16:9 (1280x720, 1920x1080). A portrait phone is
pillarboxed into that frame (vImage scale into the pool buffer), or the extension also advertises a
portrait format that browsers accept but Zoom crops. **[inferred]**

**Latency.** Enqueue to sink queue is in-process; the extension's 3×fps poll adds up to 11 ms at
30 fps; the source stream delivers on the app's capture clock. Expected total about one to two
frames; not measured. #327 owns the benchmark. **[inferred]**

**Naming.** The extension creates its devices at runtime, so it can name one "simee" camera and
retitle or add devices through custom properties set from the producer. Apps cache the device list,
so a new device can need an app restart; one fixed camera whose input `publish` switches is the
robust default. **[inferred]**

Rust bindings exist: `objc2-core-media-io` 0.3.2 has `CMIOExtension*` and `CMIOHardware*`
features **[verified, `cargo info`]**. The extension could be Rust or ~400 lines of Swift like OBS's;
its own target either way, since it is not a workspace binary.

## 2. Why not the legacy DAL plug-in

- Deprecated since macOS 12.3 (the OBS plug-in labels it "Legacy DAL (deprecated since macOS
  12.3)"). From macOS 14.1 only cameras from system extensions are listed; legacy plug-ins come back
  only after `system-override legacy-camera-plugins-without-sw-camera-indication=on` in Recovery
  Mode, which also drops the camera privacy indicator. **[verified, Apple support 108387]**
- A DAL plug-in is a bundle in `/Library/CoreMediaIO/Plug-Ins/DAL` (root to install) loaded into
  every capturing app's process, so hardened apps with library validation refuse unsigned ones.
  That is why Apple replaced it. **[inferred, WWDC22 10022]**
- This host's DAL folder holds only `plugins-info.txt`. **[verified]**

simee's floor is macOS 15, where no user gets a DAL camera without a Recovery Mode change. Not an
option.

## 3. Linux: v4l2loopback

- **Availability.** Out of tree (v0.15.4 is current). Packages: Debian/Ubuntu `v4l2loopback-dkms`,
  Fedora via RPM Fusion, Arch `v4l2loopback-dkms`, or DKMS from source. Secure Boot needs the module
  signed (DKMS enrolls a MOK) or it fails to load. **[verified README; inferred package names]**
- **Loading is root.** `modprobe v4l2loopback exclusive_caps=1 card_label="simee"` (OBS runs it
  through `pkexec`), or `v4l2loopback-ctl add -n simee /dev/videoN` on the root-only control
  device. `exclusive_caps=1` is what Chrome/WebRTC need to see the device as a camera.
  **[verified, README and OBS `linux-v4l2/v4l2-output.c`]**
- **Writing is not.** `/dev/videoN` is `root:video` 0660 under the default udev rules, so a user in
  `video` (or with a logind seat ACL) opens it. **[inferred]**
- **Frame flow.** `open(O_RDWR)`, `VIDIOC_QUERYCAP`, `VIDIOC_G_FMT`, then `VIDIOC_S_FMT` with
  `type = V4L2_BUF_TYPE_VIDEO_OUTPUT`, width, height, `pixelformat`, `sizeimage`,
  `field = V4L2_FIELD_NONE`, then one `write()` per frame (mmap streaming also works). OBS writes
  `V4L2_PIX_FMT_YUYV`; the module accepts many formats (YUYV, NV12, YUV420, RGB24, BGR32, MJPEG,
  ...). **[verified, `examples/test.c`, OBS source, `v4l2loopback_formats.h`]**
- **For simee:** emu's RGBA frames (the frame ring) to YUYV is a pure converter for
  `sim-core::pixels`, plus a writer thread. The ioctls need `unsafe` or a binding crate
  (`shiguredo_v4l2`, others); AGENTS.md lists no crate where Linux device ioctls may live, so the
  build issue decides that first. No signing, no approval. **[inferred]**

## 4. Zero-effort fallback (works today or with #266)

**OBS Virtual Camera** (macOS 13+ camera extension, Linux v4l2loopback; OBS 33.0.3 is current):

1. Run the simee desktop (`cargo run --release` in `desktop/`) on the device.
2. OBS: add a macOS Screen Capture source, method Window, the simee window (ScreenCaptureKit; OBS
   asks for Screen Recording once). On Linux, a PipeWire window capture.
3. Crop to the device with the source's crop filter; Start Virtual Camera.
4. In Zoom, Meet or a page, pick "OBS Virtual Camera".

**WHEP into OBS or a browser** (after #266 starts streams from the CLI; `sim-rtc` and `sim-encode`
already exist, `docs/streaming.md`): publish the device to a loopback MediaMTX over WHIP, then:

- for viewers, open the WHEP player page in any browser, no camera at all;
- for a call, add an OBS Browser Source on that page and start the Virtual Camera. OBS has no native
  WHEP source yet (obsproject/obs-studio#10353 is open). **[verified PR state; inferred Browser
  Source path]**

**Screen share instead of a camera.** For demos and support calls, sharing the simee desktop window
in Zoom or Meet needs no camera at all. NDI is not an option: simee has no NDI output.

These cover "demos, support calls and recording" with a one-time OBS install that is already signed,
notarized and approved by its vendor. What they do not give: per-device camera names, start from
`simee` with no GUI step, and a headless host (window capture needs the desktop on screen).

## 5. Recommendation

**Document the fallback now; build later.** The issue's second exit condition holds: the camera
extension does not fit simee's current distribution (crates.io and ad-hoc signed tarballs). Building
it first means building a signed, notarized `Simee.app` and a Homebrew cask, which is its own project
that the desktop app would also want. Until that exists, the OBS route covers the use cases. Linux is
cheap but only matters for Android devices on Linux hosts, so it rides with the camera epic's tests.

| When | Work | Estimate |
| --- | --- | --- |
| Now | Move section 4 into a user page (`docs/streaming.md` "As a camera") and link it from #319 | 0.5 day |
| Later, prerequisite | Signed `Simee.app`: Developer ID and notarytool secrets in the release workflow, bundle layout, Homebrew cask (tap) | 3 to 5 days, plus Apple team setup |
| Later | Camera extension and the producer (issues below) | 6 to 9 days |
| With #327 | Linux v4l2loopback writer | 2 to 3 days |

Issue breakdown if built:

1. **release: a Developer ID signed, notarized `Simee.app` and a Homebrew cask.** The bundle holds
   `simee` and the extension; the cask links the CLI. Prerequisite for everything below.
2. **camera: `SimeeCamera.systemextension`.** One device "simee", source and sink streams, BGRA at
   1920x1080 and 1280x720 (plus a portrait format), 30/60 fps, a placeholder frame when no sink
   feeds it; its version is independent of simee's. Activation from `Simee.app`
   (`simee camera install` opens it), reporting needs-approval with the System Settings path and
   `willCompleteAfterReboot`.
3. **camera: the sink producer (sim-coresim, macOS).** Find the camera by UID, copy the queue,
   start, enqueue pool buffers fed from `IosSource`/`AndroidSource` with `sim-encode`'s pacing;
   pillarbox to the advertised size. First check: enqueue from a non-bundle CLI, and any TCC prompt.
4. **api: `camera_publish` / `camera_unpublish` / `camera_published`.** API methods first, then
   `simee camera publish <device> [--name ...]` / `unpublish`, MCP tools; `Error::Unavailable` with
   the install hint when the extension is missing; remembered per device like #323's links.
5. **linux: v4l2loopback writer.** Find loopback devices by sysfs driver name, `S_FMT` YUYV,
   RGBA→YUYV in `sim-core`, a writer thread; settle which crate may hold the ioctls.
6. **test: getUserMedia acceptance.** Chromium with `--use-fake-ui-for-media-stream` lists "simee"
   and shows a booted device's screen (pixel-change check from #268's harness). macOS runs on a host
   with the extension approved (not hosted CI); Linux CI loads v4l2loopback if the runner allows
   modules. The frame-to-`getUserMedia` latency row goes into `docs/benchmarks.md` via #327.

## Sources

- Apple, [Creating a camera extension with Core Media I/O](https://developer.apple.com/documentation/coremediaio/creating-a-camera-extension-with-core-media-i-o) (`/Applications`, app groups, approval dialog).
- Apple, [Installing System Extensions and Drivers](https://developer.apple.com/documentation/systemextensions/installing-system-extensions-and-drivers) and [`request(_:actionForReplacingExtension:withExtension:)`](https://developer.apple.com/documentation/systemextensions/ossystemextensionrequestdelegate/request(_:actionforreplacingextension:withextension:)).
- Apple, [`com.apple.developer.system-extension.install`](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.developer.system-extension.install).
- WWDC22 10022, [Create camera extensions with Core Media IO](https://developer.apple.com/videos/play/wwdc2022/10022/).
- Apple Support, [Camera or video output after macOS Sonoma 14.1](https://support.apple.com/en-us/108387) (DAL end of life, the Recovery override).
- Apple Developer Forums: [XPC from app to CMIOExtension](https://developer.apple.com/forums/thread/706184) (sink stream, `_cmiodalassistants`), [extension stopped working in 12.3 RC](https://developer.apple.com/forums/thread/702066) (updates and reboots).
- OBS Studio, [`plugins/mac-virtualcam`](https://github.com/obsproject/obs-studio/tree/master/plugins/mac-virtualcam/src) (`obs-plugin/plugin-main.mm`, `camera-extension/OBSCameraDeviceSource.swift`, the locale strings) and [`plugins/linux-v4l2/v4l2-output.c`](https://github.com/obsproject/obs-studio/blob/master/plugins/linux-v4l2/v4l2-output.c); the WHEP source PR [#10353](https://github.com/obsproject/obs-studio/pull/10353).
- [v4l2loopback](https://github.com/v4l2loopback/v4l2loopback) README, `examples/test.c`, `v4l2loopback_formats.h`.
- SDK headers: `CoreMediaIO.framework/Headers/CMIOExtensionStream.h`, `CMIOHardwareStream.h`.
