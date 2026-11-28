# CLI

Every command is a method in the [socket API](api.md); the CLI formats its output. `<device>` is a
UDID (an Android device's is its AVD name), an exact name, or `booted`.

```bash
cargo build
alias simee=target/debug/simee

simee list                                   # every device, grouped by runtime, labelled
simee list --platform android --form-factor watch
simee form-factors
simee doctor                                 # chip, macOS, Xcode, hypervisor; each feature on or off and why (--json)
```

`doctor` isn't an API method: it reads this host itself (and the `host`
method for thermal and power) against the [support matrix](development.md#support-matrix).

## iOS

```bash
simee runtimes | device-types                # what can be created
simee create "My Phone" "iPhone 17" "iOS 26.5"                                   # phone (iOS)
simee create "Pad" "iPad Pro 11-inch (M4)" "iOS 26.5" --form-factor tablet       # iPadOS
simee create "Watch" "Apple Watch Series 10 (46mm)" "watchOS 26.0" --form-factor watch
simee create "TV" "Apple TV 4K (3rd generation)" "tvOS 26.0" --form-factor tv
simee create "Vision" "Apple Vision Pro" "visionOS 26.0" --form-factor vision
simee boot <device>                          # waits until the device finishes booting
simee rotate Phone --orientation landscape-left   # or 90° clockwise without --orientation
simee carplay Phone on                       # CarPlay display on a booted iPhone (iOS 26 runtimes)
simee tap Phone 400 240 --display carplay    # ...then touch and screenshot it
simee rotary Watch 3                         # the Digital Crown (--delta for raw crown units)
simee watch-gesture Watch double-tap         # or wrist-flick
simee button TV select                       # the Siri Remote: select, menu, home, play-pause, arrows
simee remote-swipe TV left                   # its touch surface
simee swipe iPad 1032 2748 1032 1600 --duration 150 --edge bottom   # home (slower: the app switcher)
simee swipe iPad 1960 4 1960 1200 --edge top # Control Center (top right), Notification Center (top left)
simee pointer iPad 500 520                   # the iPad's pointer: hover there (pixels)
simee pointer iPad 500 520 click             # or click; --to X2 Y2 drags
simee stylus iPad 400 1300 --to 1400 1700    # an Apple Pencil stroke (or X Y [tap|down|move|up])
simee fold Duo unfold                        # an iPhone Duo (iOS 27.1 on): the inner screen; `fold`, a posture, 0-180
simee tap Duo 1132 557                       # the screen in use; --display outer|inner names one
```

`--form-factor` is checked against the device type (an iPhone is not a tablet). CarPlay is an
external display of an iPhone simulator rather than a device type: `--form-factor auto` says so,
and `carplay on` attaches it. See [docs/form-factors.md](form-factors.md) for how each
Apple input is sent.

**Headless devices (`--set`).** Simulator.app opens a window for every device of the default
device set that boots, which slows each warm boot. `--set <dir>` (any command, or
`SIMEE_IOS_DEVICE_SET=<dir>` for every command and for `simee serve` / `simee mcp`) works in another
CoreSimulator device set, the directory `simctl --set` takes (`create` makes it; other commands
refuse one that doesn't exist); no window ever attaches to its devices, and Simulator.app's own
devices stay where they are. `--set ''` is the default set. The command's set goes with every call,
so a running server works in it whatever its own environment says.

```bash
simee --set ~/farm/sims create farm-1 "iPhone 17" "iOS 27.2"
simee --set ~/farm/sims boot farm-1          # no Simulator.app window
simee --set ~/farm/sims screenshot farm-1 shot.png
simee --set ~/farm/sims list
simee --set ~/farm/sims shutdown farm-1 && simee --set ~/farm/sims delete farm-1
```

## Android

```bash
emu -provision 36 -tag google_apis            # platform-tools, the image and the renderer
emu -list-images -tag android-wear            # other image families: android-tv, google-tv,
                                              # android-automotive, ...
simee create Pixel   --platform android --system-image "system-images;android-36;google_apis;arm64-v8a"
simee create Tablet  --platform android --system-image "system-images;android-36;google_apis;arm64-v8a" --form-factor tablet
simee create Fold    --platform android --system-image "system-images;android-36;google_apis;arm64-v8a" --form-factor foldable
simee create Duo     --platform android --system-image "system-images;android-36;google_apis;arm64-v8a" --device surface_duo
simee create Wear    --platform android --system-image "system-images;android-34;android-wear;arm64-v8a"
simee create TV      --platform android --system-image "system-images;android-34;google-tv;arm64-v8a"
simee create Car     --platform android --system-image "system-images;android-34-ext9;android-automotive;arm64-v8a"
simee create XR      --platform android --system-image "system-images;android-34;google-xr;arm64-v8a"
simee boot Pixel                             # starts emu; waits for sys.boot_completed
adb -s emulator-5554 shell                    # it is an ordinary emulator to every tool
simee fold Fold fold                         # the outer screen; `unfold`, a posture or an angle
simee rotary Wear 3                          # the rotary input (watches only)
simee xr XR recenter                         # Android XR head pose, options, pointer
simee vehicle Car gear drive                 # a vehicle HAL property (Automotive only)
simee console Pixel sensor get acceleration  # anything the emulator console speaks
simee camera sources                         # host cameras (virtual ones flagged), synthetic sources, permission
simee camera link Pixel --back qr:hello      # live on a running device, else at its next boot
simee camera link Pixel --front obs          # part of a camera's name: "OBS Virtual Camera"
simee camera links                           # every link and its state (source missing, ...)
simee camera unlink Pixel                    # both cameras back to the AVD's own (--back, --front)
simee boot Pixel --camera-back pattern:testcard --camera-front image:face.png
simee shutdown Pixel
```

Camera links are saved with the device and passed to emu on every boot, so they survive a reboot
and a server restart. A source is a source id (`qr:TEXT`, `text:TEXT`, `image:PATH`,
`solid:COLOUR`, `pattern:testcard`, `webcam:ID`, `none`) or part of a host camera's name; a name
that matches several cameras fails and lists them. iOS devices get camera links too, through a running `simee serve` (see [camera.md](camera.md)). See
[docs/camera.md](camera.md).

The form factor comes from the image (Wear OS, TV and Automotive images) or `--form-factor`
(phone, tablet or foldable from a mobile image, which also sizes the screen and adds a foldable's
hinge). An image can't boot another family's form factor, and the error says which it boots.
`emu` itself stays a drop-in for the SDK's `emulator` (`emu -avd <name>`, `make shim`); see
[docs/android.md](android.md).

## Both

```bash
simee screenshot <device> out.png            # full-resolution PNG
simee tap <device> 600 1300                  # pixels from the top-left of the screen
simee swipe <device> 600 2000 600 900 --duration 300 [--settle]
simee button <device> home|lock              # iOS: side|siri|apple-pay|action; Android: back|recents|
                                              # volume-up|volume-down|menu; TVs: up|down|left|right|
                                              # select|play-pause (menu|back on the Siri Remote)
simee rotate <device>                        # 90° clockwise (iOS: --orientation too)
simee rotary <device> 3                      # a watch's crown (Wear OS or Apple Watch)
simee type <device> 'Hello, World!'          # hardware keyboard, US layout, on both platforms
simee key <device> 40                        # HID usage (40 = Return)
simee location <device> 37.33 -122.03
simee shutdown|delete <device>
```

`<device>` is a UDID (an Android device's is its AVD name), an exact name, or `booted`.
iOS-only commands: `install|uninstall|launch|terminate|apps`, `open-url`, `appearance`, `privacy`,
`memory-warning`, `erase`, `carplay`, `watch-gesture`, `remote-swipe`, `pointer`
(and `swipe --edge`); Android-only:
`console`, `xr`, `touchpad`, `vehicle`, `camera link|unlink|links` (`fold` works on Android foldables and the iPhone Duo). `stylus` works on both: a pen on the touch screen
(`simee stylus <device> 400 300 hover`, `... down --pressure 200 --button`, `... 100 100 --to 900 600`
for a stroke, `--eraser` for the eraser end); on an iPad it is the Apple Pencil, which takes taps,
`down`/`move`/`up` and strokes but no hover, pressure, button or eraser (the simulator's touch
screen has none). Using one
on the other platform's device fails with the platforms it works on.


## Streams

```bash
simee stream start booted --whip http://127.0.0.1:8889/phone/whip --token-file ~/.mtx-token [--fps 30] [--max-size 1280] [--bitrate 2500000] [--input]
simee stream status <stream_id>     # state, stage timeline, stats; renews the lease
simee stream list [device]          # the server's streams, without renewing anything
simee stream watch <stream_id>      # renews the lease every third of it; Enter stops the stream
simee stream stop <stream_id>       # ends it and sends the WHIP DELETE
```

A stream lives in the process that started it and ends, sending its WHIP DELETE, only if that
process stops it, so `stream` commands always go to a running `simee serve` of this build and
refuse (also with `--in-process`) when none answers. `start` prints the `stream_id`, the
`whip_resource` and the lease (30 s); a stream nobody renews with `status` or `watch` is stopped
when its lease lapses. Give the bearer token with `--token-file` (the file's trimmed contents) or
`SIMEE_STREAM_TOKEN`: `--token` puts it in the process list. The token is never printed. Every
command takes `--json`, except `watch`. Details in [streaming.md](streaming.md).

## Socket API

Everything above is a method in `sim-api`'s table, served as JSON lines on a unix socket; the CLI
and the desktop window call the same table.

```bash
simee serve                                   # $SIMEE_SOCKET or $TMPDIR/simee.sock
simee call help                               # every method, with the platforms it supports
simee call devices '{"platform":"android"}'
simee call fold '{"device":"Fold","posture":"half-opened"}'
simee call ca_debug '{"device":"booted","option":"blended-layers","on":true}'
```

The desktop window serves the same API on `$TMPDIR/simee-desktop.sock`. Full reference:
[docs/api.md](api.md).

## MCP server

`simee mcp` serves [MCP](mcp.md) on stdin and stdout: every method above as a tool for an agent,
with resources for devices and their screens, tasks for boots, and a device view. It runs the
methods in its own process, like `--in-process`; configure it in the agent as the command `simee
mcp`. It serves the `core` toolset (about twenty tools) unless `SIMEE_MCP_TOOLSETS` names others
(`core,debug`, or `all`): see [toolsets](mcp.md#toolsets).
