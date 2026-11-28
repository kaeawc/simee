# simee API reference

Generated from `simee call help`. Transport: unix socket, one JSON request per line, one JSON response per line.

```text
→ {"id": 1, "method": "tap", "params": {"device": "booted", "x": 600, "y": 1300}}
← {"id": 1, "ok": true, "result": null}
```

A connection may send any number of requests, each answered before the next is read; keep one open
rather than connecting per call. Large results (a base64 `screenshot`) are written as one line
straight from the result, so read up to the newline before parsing.

**Per-call overhead** (#202). Measured with `cargo run --release -p simee-api --example api_overhead --
before=<old simee> after=target/release/simee`: a private `simee serve` and `simee mcp` per binary,
rounds interleaved, the median of 5 (Apple M3 Max, 16 cores, 2026-11-27, host load 19 to 22):

| µs per call | kept connection | connection per call | MCP (stdio) |
|---|---|---|---|
| `identity` (no-op) | 7.2 | 22.2 (33.5 before) | 18.3 |
| `form_factors` (6 KB) | 49.4 | 66.5 (78.9 before) | 132.4 |
| `help` (25 KB) | 88.6 | 110.7 (123.2 before) | 211.8 |
| `devices` (no boot) | 75 560 | 73 930 | 75 210 |

- On a kept connection the server spends under 1 µs on a no-op (parse, dispatch, response line);
  the rest is the socket round trip. A larger result costs building it and writing it
  (`cargo test --release -p simee-api call_overhead -- --ignored --nocapture` splits it up).
- A new connection costs about 15 µs more: the connection thread is reused (a thread whose client
  disconnected waits 30 s for the next, at most 8 wait), and its session is still made new for it.
- MCP adds JSON-RPC framing, the tool result's text and `structuredContent`, and handing each call
  to a worker thread (`cargo test --release -p simee-mcp tool_call_overhead -- --ignored --nocapture`).
- `devices` on a host with Android AVDs is the Android listing, not the API: it reads each AVD's
  lock and asks the kernel about the process that holds one. An exact AVD name resolves with one
  `stat` and `device: "booted"` loads only the running AVDs (`docs/benchmarks.md` "Android device
  resolution").

**Binary replies.** A request may add `"binary": true` to take a result's bytes raw instead of as
base64 in the line (a local client that wants the PNG skips the base64 encode, the JSON escaping and
a third more bytes on the socket). When the result has a bytes field (today `screenshot`'s `png`
without `path`), the response line carries `"binary": <len>`, the field holds `{"binary": <len>}`,
and exactly `<len>` raw bytes follow the newline before the next response:

```text
→ {"id": 3, "method": "screenshot", "params": {"device": "booted"}, "binary": true}
← {"binary":4383251,"id":3,"ok":true,"result":{"height":2622,"png":{"binary":4383251},"width":1206}}\n
← <4383251 bytes of PNG>
```

- It is asked per request, so connections stay stateless, a client can mix both kinds, and every
  request without the field (every existing client, MCP) gets the same bytes as before.
- No bytes field (any other method, or `screenshot` with `path`), or an error: the normal response
  line, with no `binary` and nothing after it. A server from before binary replies ignores the
  field and answers with base64, so a client that asks should accept either.
- Read the line, then exactly `<len>` bytes: a stream that ends first is an error (the Rust client,
  `sim_api::Client::call_binary`, says `the reply ended after N of <len> bytes`).
- A reply carries at most one attachment. `read`'s `base64` is not a bytes field yet: it stays base64.
- `simee screenshot <device> -` writes the PNG to stdout this way (in its own process, the bytes are
  never base64 either). `simee screenshot <device> <path>` keeps sending `path`: whichever process
  runs the method writes the file, so the PNG doesn't cross the socket at all.
- The MCP server stays base64: its tool results are JSON text.

**The CLI and a server.** When a matching `simee serve` is listening on the default socket
(`$SIMEE_SOCKET`, else `$TMPDIR/simee.sock`), other `simee` commands run their method on that
server instead of in their own process: they skip connecting to the simulator service, reuse the
server's per-device HID and display connections, and drop the CLI's 100 ms post-input wait (the
server's HID client outlives the command). The output is byte for byte the same.

- **Handshake.** Before forwarding, the CLI calls `identity` with a 1 s deadline for connecting and
  answering. It forwards only when the answer equals its own: the same build (crate version and a
  hash of the method table), the same executable (on macOS its Mach-O `LC_UUID`, which the linker
  derives from the linked code: changed code never matches, a relink or copy of the same code
  does; elsewhere path, inode, size and mtime unchanged since the server started), and the same
  environment (a hash of every `ANDROID_*`, `EMU_*` and `SIMEE_*` variable, `DEVELOPER_DIR`,
  `HOME`, `TMPDIR`, and the `adb` that `PATH` finds, which the Android backend falls back to). `PATH`
  itself is not part of it (a pyenv shim or an extra directory changed it without changing any
  result), nor are the client's own switches (`SIMEE_SOCKET`, `SIMEE_IN_PROCESS`,
  `SIMEE_REQUIRE_SERVER`, `SIMEE_VERBOSE`, `SIMEE_E2E_*`).
- **Fallback.** No server, a stale socket file, another build or environment, a server from before
  the handshake (it answers "unknown method"), or one that doesn't answer in time (suspended,
  wedged): the command runs in the CLI's own process. A forwarded command has no timeout after the
  handshake (a boot can take minutes). `--in-process` (or `SIMEE_IN_PROCESS=1`) skips the server
  altogether. Running here costs several times more per call (screenshot 150 ms instead of 30 ms,
  tap 316 ms instead of 78 ms, measured), so a farm can see or forbid it: `--verbose` (or
  `SIMEE_VERBOSE=1`) prints where each command ran and why the server wasn't used
  (`simee: `tap` runs in this process: the server on … can't be used: the server is another build
  or environment: …`), and `SIMEE_REQUIRE_SERVER=1` makes such a command fail with that reason
  instead of running here (methods that never forward still run here).
- **Never forwarded** (`sim_api::IN_CALLER`): `boot` and `boot_many` (on Android both start `emu` as a child of the
  calling process; on a server it would be the server's child, in its process group, killed by
  Ctrl-C on `simee serve` and never reaped), `network_shape`, `network_state`, `network_flows`,
  `network_clear` (the shaping proxy is a thread of the process that starts it),
  `audio_capture_start`, `audio_capture_stop`, `audio_mute` and `audio_state` (a capture or mute is
  a helper process its caller owns). These always run in the caller, and need no handshake.

**Stopping a server** (#388). On SIGINT (Ctrl-C) or SIGTERM, `simee serve` shuts down in order.
A call in flight stops at its next safe point and gets up to 10 s. Every call after the signal,
on a connection already open or one made during the shutdown, isn't run. Both answer
`cancelled: server shutting down`. Then it stops every stream with its WHIP DELETE, removes its
socket and its camera feed sockets, and exits 130 (SIGINT) or 143 (SIGTERM). It keeps listening
until it has removed its socket, so another `simee serve` can't take the path meanwhile. A second
signal exits at once. From the signal on, `call_cancelled` is true for every call in the process.

A shutdown leaves alone the devices the server didn't start, but it can stop or delete ones it
did:
- Ctrl-C at a terminal goes to the whole process group, so every Android emulator the server
  started as its child gets the SIGINT too, powers off and exits 130. A SIGTERM sent to the
  server's PID alone doesn't reach them.
- A call cancelled at a safe point undoes what it made: a `create` or `clone` that finishes
  during the drain is deleted, and a `boot` waiting for admission boots nothing.

`simee mcp` stops its streams and exits the same way, without waiting for its calls; a watched
`boot` that launched its device for a call in flight may shut that device down as it is
cancelled. `emu` stops its guest as the console's `kill` does and exits 130 or 143 too.

Relative `path` arguments (`screenshot`, `install`) are made absolute before they are sent.

`device` is a UDID (an Android device's UDID is its AVD name), an exact name, or `booted` (the default: the only running device). It is resolved across both platforms; pass `platform` (`ios` or `android`) to look at one. Parameters ending in `?` are optional.

**Platforms** says whose devices a method works on. Calling a method on a device of another platform fails with the platforms it does work on, e.g. `` `status_bar` isn't available on Android devices (it works on iOS) ``. Every device in a result carries `platform`, `form_factor` and `sub_platform`.

| Method | Platforms | Parameters | Description |
| --- | --- | --- | --- |
| `help` | Android, iOS | — | List methods with the platforms each supports |
| `identity` | Android, iOS | — | This process's build, executable and environment (hashed): the CLI forwards a command to a server only when they match its own |
| `host` | Android, iOS | — | Host health, sampled without forking (`simee-host`): load averages and CPU count; memory pressure (macOS: the kernel's `kern.memorystatus_vm_pressure_level` and `kern.memorystatus_level`; Linux: MemAvailable and PSI, `psi` null when the kernel has it off) and swap; free disk on the data volume; thermal (macOS: `NSProcessInfo.thermalState`; Linux: thermal zones with trip margins, the cpufreq cap, Intel throttle counters; `available: false` and null fields when a host has none); power (source, battery charge, charging, time remaining, Low Power Mode or the ACPI platform profile, cpufreq governor; the macOS energy mode is unavailable because only `pmset` reads it); 1/5/15-minute averages from a 1 s ring (including samples spent in each thermal state and throttle events); the load trend; and a verdict (`admit`, `hold`, `backoff`) with `reason` (the deciding signal) and `reasons` (every signal, each prefixed `load:`, `memory:`, `thermal:` or `power:`). Thermal `serious` holds and `critical` backs off; on battery or Low Power Mode holds, and under 20% charge on battery backs off; thresholds (load 1.5 per CPU, 8 GiB hold, 4 GiB backoff) are spectra's, with memory judged by the kernel's level on macOS. A CLI call has one sample; a server (`simee serve`) keeps the history while something reads it: the sampler runs only while `host`, `devices_usage`, an admission check or the desktop's panel read it, and parks 60 s after the last read (an idle server samples nothing). A read on a parked sampler takes one sample at once (two, 250 ms apart, on a Linux host that counts throttle events, so a throttled host holds on its first check) and starts the history over, so the averages cover only the time since, and `history_note` says so (set while under 15 minutes of history; the verdict then rests on the newest sample and the kernel's own load average) |
| `devices_usage` | Android, iOS | — | Which running devices load the host, from the same 1 s sampler as `host` and without forking (`simee-host`'s `usage`; one libproc pass over the process table per sample, about 0.7 ms with ~1300 host processes): each device's processes are an iOS simulator's `launchd_sim` tree (its UDID from the `launchd_bootstrap.plist` path), a simee `emu` started for an AVD with its children, or a stock emulator's `qemu-system-*` that names an AVD, runs from the SDK's `emulator/qemu/` or has an `emulator` launcher above it (with its children and launchers; a bare qemu from colima or lima is not a device). Each row has `id` (UDID or AVD name), `name`, `platform`, `form_factor`, `sub_platform`, `kind` (`simulator`, `emu`, `emulator`), `pid` (the root), `pids`, `cpu_pct` (percent of one core, `{"1s", "1m"}`, from cumulative CPU-time deltas: a call never sleeps to measure), `resident_bytes`, `footprint_bytes` (Activity Monitor's Memory: compressed and swapped pages included; libproc has no per-process compressed size), `disk_read_bytes_per_s` and `disk_write_bytes_per_s`, and `energy_w` (watts from the kernel's per-process energy counter; null where it has none, and 0 in a macOS VM, whose counters stay at zero); rows are costliest first (1-minute CPU, then footprint). `simee` is simee's own processes (`simee` serve/CLI/MCP, `simee-desktop`, an `emu` that runs no AVD, and what they started that isn't a device); `host` has `cpus`, `cpu_pct` (busy, percent of one core), `memory_total_bytes`, the devices' totals and `devices_share_pct` / `simee_share_pct` of the busy CPU. A pid is one process only with its start time: a reused pid is a new process, and one that started since the last sample counts whole. A device younger than a minute is rated over its life; `window_s` says what the windows span. The first call after the sampler starts (a CLI call, the first read of a `simee serve`, or the first after it parked) waits for the second sample; macOS only (`available: false` elsewhere) |
| `form_factors` | Android, iOS | platform?, form_factor? | Every form factor on every platform: sub-platform, inputs and reference profile, or why it's unsupported |
| `devices` | Android, iOS | booted?, platform?(android\|ios), form_factor? | List devices with their platform, form factor and sub-platform |
| `runtimes` | Android, iOS | platform? | List installed runtimes (Android: system images) |
| `device_types` | iOS | — | List device types |
| `create` | Android, iOS | name, platform?(ios\|android), form_factor?, admission?(ignore\|refuse); iOS: device_type, runtime; Android: system_image, device?(pixel_fold\|surface_duo) | Create a device. form_factor is checked against the device type (iOS) or the system image (Android) and shapes a new AVD. Returns the device with `admission`, what booting it would do to the host (as `admission_check`); admission refuse fails fast, creating nothing, unless the verdict is admit |
| `delete` | Android, iOS | device | Delete a device |
| `boot` | Android, iOS | device, timeout_s?, admission?(ignore\|refuse\|wait), admission_timeout_s?; iOS: env?, shaped?, settle?, settle_cpu_pct?, settle_window_s?, settle_foreground?(true), settle_timeout_s?, profile?("slim"); Android: audio?(false), camera_back?, camera_front? | Boot and wait until finished (iOS: env is the device launchd's environment, shaped injects the network shaper; settle also waits up to settle_timeout_s (600) until the device's processes use under settle_cpu_pct (10) % of a core for settle_window_s (10) s (settle_foreground, the default: counting only foreground-QoS CPU) and returns the timings with cpu_pct, foreground_cpu_pct, background_cpu_pct and cpu_s (CPU-seconds the tree used since the boot; for a device already booted, since the first sample), settled false if it ran out of time; profile turns off a named set of daemons (written before the boot of a shut-down device, so they never start); Android: starts emu and waits for sys.boot_completed, silent unless audio, which plays the device on the host's default output; camera_back and camera_front link a camera source first, as camera_link does, saved for later boots too). Returns `admission`, what the device would do to the host (as `admission_check`; nothing for one already booted): admission ignore, the default, only reports it; refuse fails fast unless the verdict is admit; wait waits until it is (at most admission_timeout_s, 600) and returns `pacing` |
| `boot_many` | Android, iOS | devices, concurrency?, timeout_s?, admission?(ignore\|refuse\|wait), admission_timeout_s?; iOS: env?, shaped?, settle?(default true), settle_cpu_pct?, settle_window_s?, settle_foreground?(true), settle_timeout_s?, profile?("slim"); Android: first_port?(5554) | Boot several devices of one platform, at most `concurrency` at once (default: a quarter of the host's CPUs, at least 2), each as `boot` does (iOS: a slot is held until the device settles, unless settle is false); returns each device's timings or error, failed and unsettled counts, and the time until all were done. Android: each on its own console port from first_port up. Returns `admission` for the devices not running yet (as `admission_check`): admission ignore, the default, only reports it; refuse fails fast unless the verdict is admit; wait paces the boots, starting as many as the host admits and asking again every 5 s, and returns `pacing` (each start and wait); devices not admitted within admission_timeout_s (600) are not started and fail with the reason |
| `admission_check` | Android, iOS | platform(ios\|android), form_factor?(phone), count?(1) | Whether the host can take `count` more devices of a platform and form factor now: each one's predicted cost while booting and idle (CPU % of a core and memory: docs/benchmarks.md's numbers until this process has seen idle devices of the kind running, then this host's), what they would do to memory used and the 1-minute load, and an admit/hold/backoff verdict (on an idle sampler the check takes one sample, or two 250 ms apart where Linux counts throttle events, and judges that with the kernel's own load average; `host_basis` says so) (the `host` verdict, which still applies, and the prediction: hold at 85% memory used or the host's load limit, back off at 95%) with every reason and how many of them are admissible now. `boot`, `boot_many`, `create` and `clone` return the same as `admission`. See [Admission](#admission) |
| `settle` | iOS | device, cpu_pct?, window_s?, foreground?(true), timeout_s?(600) | Wait until a booted simulator is quiet: its processes under cpu_pct (10) % of a core for window_s (10) s (foreground, the default: only foreground-QoS CPU counts), SpringBoard running; returns settle_s, cpu_pct, foreground_cpu_pct and background_cpu_pct, and cpu_s and background_cpu_s (CPU-seconds used since the first sample) |
| `profile` | iOS | device, name(slim), undo? | Turn a named set of daemons off on a booted simulator (disabled in its launchd database and stopped: stays off across reboots), or with undo back on (from the next boot); returns how many changed, those that failed and those that weren't loaded |
| `clone` | Android, iOS | device, name, admission?(ignore\|refuse) | Create a device as a copy of a shut-down one. iOS: a simulator (data, settings, apps; APFS clones of its files); Android: an AVD from a golden that has booted under simee (copy-on-write clones of its disks). A booted-once template's copies skip the first-boot work. Returns the copy with `admission`, as `create` does |
| `export` | Android | device, to | Pack a shut-down device into a portable bundle at `to` (a .tar file, else a new directory) with no host path in it: manifest.json and content-addressed chunks (1 MiB, BLAKE3, each LZ4-compressed; zero ranges stored as extents). Android: an AVD that has booted under simee (its simee disks, config.ini without host paths, boot.prop) and its system image's identity; captures disks, not RAM (resume is a cold boot). Returns sizes, the identity and the secrets inventory. iOS simulators: not yet (the next step of #170) |
| `import` | Android | from, name, allow_mismatch?(false) | Create device `name` from a bundle `export` wrote. Android: finds the system image by content (API, tag, ABI, revision, BLAKE3 of its images, fingerprint) and refuses, naming what to install, when none matches (allow_mismatch: use another installed revision of the same API, tag and ABI, named in `mismatch`); checks every chunk and disk hash; drops config.ini keys holding host paths (listed in `dropped`); writes nothing unless it all succeeds. The import keeps the exported device's guest identity (ro.serialno, android_id). iOS: not yet |
| `shutdown` | Android, iOS | device | Shut down |
| `erase` | iOS | device | Erase content and settings (shut down first) |
| `screen` | Android, iOS | device, display?(main\|outer\|inner\|carplay) | Screen size in pixels, the space tap, swipe and touch take (a folded Android foldable: the size its guest lays out for, a Pixel Fold's 1080x2092, as uiautomator's bounds). display (iOS): main is the screen in use (an unfolded iPhone Duo's inner one), outer and inner an iPhone Duo's two, carplay an iPhone's CarPlay screen |
| `screenshot` | Android, iOS | device, path?, display?(main\|outer\|inner\|carplay) | PNG to path, else base64 in `png` (a folded Android foldable: its outer screen as the scanout shows it, a Pixel Fold's 949x1840; an iPhone Duo's inner screen as its framebuffer holds it, 2007x2853) |
| `stream_start` | Android, iOS | device, whip_url, token?, fps?(30), max_size?(1280), bitrate?(2500000), audio?(false), input?(false) | Stream the screen as H.264 over WebRTC to a WHIP endpoint (plaintext http to a loopback host only, e.g. MediaMTX's `http://127.0.0.1:8889/<path>/whip`), with token as its bearer token; max_size caps the longest side (0: full size), bitrate is in bit/s; input offers the `simee-input` data channel, whose touches, keys, text, buttons, rotations, folds and scrolls reach the device (a server that refuses the data channel, as MediaMTX does, gets the stream without it: the result's input is false and input_note says why). One stream per device and URL, living in this process (`simee serve`) on a 30 s lease that stream_status renews: a stream nobody renews is stopped. Returns stream_id, whip_resource and lease_ttl_s (and, with input, whether it is on). Encoded on macOS by VideoToolbox (iOS and Android devices) and on Linux by OpenH264 in a build with `--features openh264` (Android devices only); audio isn't streamed yet |
| `stream_status` | Android, iOS | stream_id | A stream's state (starting, running, ended and why), its stages in the order reached (requested, first_encoded_frame, whip_connected, ended; ms since requested) and stats (encoded, sent, keyframes, bytes, fps, bitrate_bps, the publisher's bitrate_estimate_bps, the viewers' pli and fir keyframe requests, input events and errors); renews its lease |
| `stream_stop` | Android, iOS | stream_id | Stop a stream: ends its encode loop and sends the WHIP DELETE; its last status, with deleted |
| `streams` | Android, iOS | device? | This process's streams (or one device's), each as stream_status shows it, without renewing a lease |
| `contrast_check` | Android, iOS | device, region?, scale?, appearance?(current\|both), restore?(light\|dark), settle_ms?(500), path?, display?(main\|outer\|inner\|carplay) | Text and controls below WCAG contrast in a screenshot, from pixels alone: finds text-like regions, estimates each one's foreground and background, and reports those under 4.5:1 (normal text) or 3:1 (large text of about 18 pt, icons and controls) with their box, the two colours and the ratio, and the screenshot with the findings boxed (`png`, or written to path). Text over a photo or a busy background has no one background and isn't judged. appearance both (iOS) checks the light and the dark UI (switches, waits settle_ms, checks) and leaves the device in restore (default light); Android can't switch appearance yet. region is {x, y, width, height} in pixels; scale is pixels per point (default: the device's, else 1) |
| `tap` | Android, iOS | device, x, y, hold_ms?, display?(main\|outer\|inner\|carplay) | Tap at pixels, held hold_ms (default: iOS 10, Android 60) (display, iOS only: an iPhone Duo's outer or inner screen, an iPhone's CarPlay screen) |
| `swipe` | Android, iOS | device, x1, y1, x2, y2, duration_ms?, settle?, display?(main\|outer\|inner\|carplay), edge?(top\|bottom\|left\|right; iOS) | Swipe between pixels over duration_ms (default 300; iOS sends its samples on schedule, so a busy host doesn't stretch it); with edge (iOS) the touch comes in from that screen edge, as a drag from Simulator.app's bezel: home and the app switcher from the bottom, Notification Center and Control Center from the top (Android's edge gestures need only the start point) |
| `touch` | Android, iOS | device, x, y, phase(down\|move\|up), display?(main\|outer\|inner\|carplay), edge?(top\|bottom\|left\|right; iOS) | One raw touch sample |
| `button` | Android, iOS | device, button(home\|lock\|side\|siri\|apple-pay\|action \| back\|volume-up\|volume-down\|recents\|menu\|up\|down\|left\|right\|select\|play-pause), hold_ms? | Press a hardware button, as the device's form factor has it: the Siri Remote on Apple TV (menu/back, home, select, play-pause, arrows) and the D-pad on Android TV; on Apple Watch home is the crown's press. Held hold_ms (default: 10 for home and lock on an iPhone or iPad, else 80) |
| `type` | Android, iOS | device, text | Type on the hardware keyboard (iOS: a key report every 3 ms) |
| `key` | Android, iOS | device, usage | Press a key by HID usage (iOS: down and up 3 ms apart) |
| `open_url` | Android, iOS | device, url | Open a URL (Android: `am start -a VIEW`) |
| `install` | Android, iOS | device, path | Install an .app (Android: an .apk with `adb install -r`, an .aab through bundletool when it is on the PATH) |
| `uninstall` | Android, iOS | device, bundle_id | Uninstall an app (Android: the package name; `pm uninstall`) |
| `launch` | Android, iOS | device, bundle_id, restart?; iOS: args?, env?, shaped?, malloc_stack_logging? | Launch (iOS: with arguments and environment, shaped injecting the network shaper, malloc_stack_logging setting MallocStackLogging=1 for mem_leaks' stacks and mem_history; Android: the package's launcher activity); restart: terminate a running instance first, default true (Android: `am start -S`); false brings a running app to the front; returns pid (Android: and the `activity` it started) |
| `terminate` | Android, iOS | device, bundle_id | Terminate an app (Android: `am force-stop`) |
| `apps` | Android, iOS | device; Android: all? | Installed bundle ids (Android: the third-party packages, with all the system's too) |
| `appearance` | iOS | device, mode(light\|dark) | Light or dark UI |
| `location` | Android, iOS | device, latitude, longitude | Simulated location |
| `privacy` | iOS | device, action(grant\|revoke), service, bundle_id | Privacy permission |
| `push` | iOS | device, bundle_id, payload | Deliver an APNs payload (object) |
| `status_bar` | iOS | device, time?, data_network?, wifi_mode?, wifi_bars?, cellular_mode?, cellular_bars?, operator?, battery_state?, battery_level? | Override status bar |
| `clear_status_bar` | iOS | device | Remove status bar overrides |
| `content_size` | iOS | device, size | Dynamic Type size (name or 0-11) |
| `increase_contrast` | iOS | device, on | Increase Contrast |
| `dynamic_island` | iOS | device, suppressed | Hide the Dynamic Island |
| `backlight` | iOS | device, on | Display backlight |
| `keyboard_language` | iOS | device, language | Software keyboard language |
| `hardware_keyboard` | iOS | device, on | Connect the hardware keyboard |
| `ca_debug` | iOS | device, option, on | Render overlay (see ca_debug_state for names) |
| `ca_debug_state` | iOS | device | Every render overlay and whether it is on |
| `reset_keychain` | iOS | device | Delete every keychain item |
| `memory_warning` | iOS | device | Simulate a memory warning |
| `notify` | iOS | device, name, state? | Post a Darwin notification (optionally setting its state) |
| `biometric` | iOS | device, action(match\|nomatch\|enroll\|unenroll), face? | Touch ID / Face ID |
| `exec` | iOS | device, program, args?, env? | Run a program inside the device; returns stdout/stderr |
| `logs` | iOS | device, last?, predicate? | Unified log lines (default last 1m) |
| `processes` | iOS | device | Device processes with CPU and memory |
| `kill` | iOS | device, pid, signal? | Signal a device process |
| `connections` | iOS | device | Open sockets of device processes |
| `traffic` | iOS | device | Bytes in/out per device process |
| `mem_leaks` | iOS | device, bundle_id \| pid, raw?, timeout_s?(120) | Leaked memory of a running app (the host's `leaks`): totals, and the leaks grouped by type and allocation stack, most bytes first, each with its instances and stack (innermost frame first; launch with malloc_stack_logging for stacks). raw adds the report as printed. See [Memory tools](#memory-tools-mem_) |
| `mem_heap` | iOS | device, bundle_id \| pid, raw?, timeout_s?(120) | A running app's heap by class (the host's `heap`): zones, node and byte totals, and every class with its count, bytes and average size, most instances first. raw adds the report as printed |
| `mem_vmmap` | iOS | device, bundle_id \| pid, raw?, timeout_s?(120) | A running app's memory regions (the host's `vmmap --summary`): each region type's virtual, resident, dirty, swapped, volatile and empty bytes and count, the malloc zones with allocations and fragmentation, and the totals. raw adds the report as printed |
| `mem_history` | iOS | device, bundle_id \| pid, address, raw?, timeout_s?(120) | The allocation stack of an address in a running app (the host's `malloc_history`), as events with their stacks; needs the app launched with malloc_stack_logging (MallocStackLogging=1 logs live allocations only). raw adds the report as printed |
| `containers` | iOS | device, sizes? | App data and bundle containers |
| `ls` | iOS | device, path? | List a directory under the device data dir |
| `read` | iOS | device, path | Read a file (base64) |
| `write` | iOS | device, path, text? \| base64? | Write a file |
| `rm` | iOS | device, path | Remove a file or directory |
| `du` | iOS | device, path? | Size in bytes |
| `clear_container` | iOS | device, path | Empty a container (an app's data) without deleting it |
| `defaults_read` | iOS | device, domain | Read a preferences domain |
| `defaults_write` | iOS | device, domain, key, value, type? | Write a preference (type: string\|int\|float\|bool) |
| `prefs_stores` | Android, iOS | device, bundle_id | An app's preference stores, each with the id the other prefs_* methods take: Android `shared_prefs/<name>.xml` and DataStore `datastore/<name>.preferences_pb`, iOS the app's domain (its bundle id) and its app-group suites; any other iOS domain is reachable by its name |
| `prefs_read` | Android, iOS | device, bundle_id, store?, fresh? | Every key of an app's preference stores (or one) with its typed value, {type, value}: string, int, long, float, double, bool, string_set (Android), date (ISO 8601), data (base64), array and dictionary (iOS; of typed values), and the read-only unsupported {native, raw} for what no type holds exactly. iOS reads integers as long and reals as double. Without store, each store answers {store, values} or {store, error}. fresh (iOS): read through cfprefsd, one in-device call, for writes it hasn't flushed to disk yet (it flushes about every 10 s) |
| `prefs_write` | Android, iOS | device, bundle_id, store, key, value, type(string\|int\|long\|float\|double\|bool\|string_set\|date\|data\|array\|dictionary) | Set one key to a typed value (refused when the value isn't of its type or the store can't hold the type); visible says whether the app sees it now or after_relaunch, app what was done to the app for it (untouched, stopped, relaunched: a running Android app writes its old values back over the file), with a note on why |
| `prefs_delete` | Android, iOS | device, bundle_id, store, key | Remove one key; visible and app as prefs_write's |
| `prefs_batch` | Android, iOS | device, bundle_id, store, ops([{key, type, value} \| {key, delete: true}]) | Set and remove several keys of one store, all or nothing: every op is checked first (each key once), then the store is written once or not at all (iOS: a batch refused up front changes nothing, but cfprefsd applies an accepted one key by key, so a reader at that moment can see part of it; if the final synchronize fails after the keys were handed over, the error names them and says some may already be live in the running app) |
| `prefs_watch` | Android, iOS | device, bundle_id, store?, cursor?, wait_ms? | Changes to an app's preference stores (or one), as `logs` stream does: the first call starts a feed in this process and returns a cursor; a call with it returns the changes since, {store, key, old, new, at}, oldest first, waiting up to wait_ms (at most 60000) for one, and the next cursor (reset: the feed restarted, re-read; missed: changes dropped unread past its 16 MiB; ended: the device stopped or nobody read it for 5 minutes, and a call without a cursor starts anew) |
| `crash_reports` | iOS | device | Host crash reports for this device |
| `info` | Android, iOS | device | Paths and identity |
| `network_shape` | iOS | device, preset?(offline\|3g\|edge\|lte\|lossy\|very-bad\|none), offline?, latency_ms?, down_kbps?, up_kbps?, loss_pct?, port_base?, port_count?, rules?([{pattern, offline?, latency_ms?, down_kbps?, up_kbps?, loss_pct?}]), proxy?(default true), inject?(false\|true\|"launchd"\|"boot") | Shape the device's network without root. proxy: a shaping HTTP/SOCKS proxy set in the device's configd_sim. inject: an in-process socket shaper; "boot" reboots the device with it in launchd's environment so every process, WebKit included, is shaped live by the profile |
| `network_state` | iOS | device | Current profile, proxy port and injection state |
| `network_flows` | iOS | device | Connections through the device's shaping proxy (host, bytes, duration, outcome) |
| `network_clear` | iOS | device | Remove shaping: clear the device proxy, stop the proxy, remove injection |
| `audio_state` | iOS | device | Device audio settings (routed host UIDs, volume) and every host audio device |
| `audio_volume` | iOS | device, level(0-100) | Device output volume (100 = unity); applies to audio clients started afterwards |
| `audio_route` | iOS | device, scope(input\|output), uid \| "default" | Pin the device's input or output to a host device (any device, including loopback); applies to clients started afterwards |
| `audio_capture_start` | iOS | device, path?(.wav), mute? | Capture this device's audio output with a process tap (no driver); mute silences it on the host meanwhile; returns capture_id |
| `audio_capture_stop` | iOS | capture_id | Stop a capture: frames, duration_ms, rms, peak, dbfs, path |
| `audio_mute` | iOS | device, on | Silence (or restore) this device's audio on the host: a per-device null sink |
| `camera_sources` | Android, iOS | kind?(host\|virtual\|synthetic) | The sources a device camera can link to, each with the `source` id camera_link takes: the host's cameras (built-in, external, Continuity Camera, and virtual cameras such as OBS Virtual Camera, flagged `virtual`) and the synthetic kinds (pattern:testcard, qr:, text:, image:, video: on macOS, solid:); and the host's camera permission (macOS: the TCC status of the app that started the server, with the fix when it blocks capture), read without ever prompting |
| `camera_link` | Android, iOS | device, facing(back\|front\|both), source | Feed a device camera from a source: a source id (webcam:<id>, pattern:testcard, qr:<text>, text:<text>, image:<absolute path>, video:<absolute path> (macOS, looped), solid:<colour>; emulated is the test card, none turns the camera off) or a host camera's name, index or part of its name (obs finds OBS Virtual Camera; an ambiguous one fails listing the matches), saved by its stable id. Saved with the device: it comes back on every boot and survives a server restart. Android: a running device is relinked live through emu's console (`camera link`), a shut-down one gets it at boot (-camera-back, -camera-front); a camera that was off at boot appears only after a reboot (the guest lists its cameras once). iOS: injects the camera shim device-wide and serves the source from the running server (needs `simee serve`; one feed per device, so back and front share it; apps already running need a relaunch) |
| `camera_unlink` | Android, iOS | device, facing?(back\|front\|both) | Remove a device camera's link (default both): it goes back to the AVD's own camera (config.ini hw.camera.*), live on a running device and at every boot |
| `camera_links` | Android, iOS | device? | Each linked device camera (one device, or every device): facing, source (and a host camera's name), live (what the running emulator shows) and state: linked, saved (applies at the next boot), relinked (changed from the console; the next boot restores the saved link), unsaved (linked from the console only), off, source missing (the host no longer lists the camera: the device shows the test card until it is back) or permission missing, with the fix |
| `network_env` | iOS | device | Environment that makes one app launch shaped (for simctl: prefix each key with SIMCTL_CHILD_) |
| `console` | Android | device, command \| commands | Emulator console commands (`adb emu`'s protocol: sensor, geo, sms, gsm, finger, power, ...); reply lines |
| `rotate` | Android, iOS | device, orientation?(portrait\|landscape-right\|portrait-upside-down\|landscape-left; iOS) | Rotate the device 90° clockwise, or to an orientation (iOS) |
| `fold` | Android, iOS | device, folded(true\|false) \| angle(0-180; 0-360 dual-screen) \| posture(closed\|half-opened\|opened\|flipped\|tent) | Set a foldable's hinge. Android: as the console's fold, unfold and posture do (the hinge sensor, the posture and, folded, the outer screen). iOS: an iPhone Duo's (closed, half-opened is the 100° book posture, opened), which SpringBoard sweeps to, moving the content to the inner screen when it opens past its swap boundary. Returns angle, posture, folded and the size the guest lays out for (iOS: of the screen in use, named in display) |
| `xr` | Android | device, action(recenter \| rotate\|move: x, y, z \| options: environment(day\|night), passthrough(0-1) \| input: mode(mouse\|hand\|eye) \| viewport: mode(pan\|zoom\|rotate) \| pointer: x, y, phase?(move\|down\|up\|click) \| hand\|eye: x, y, press?(aim\|press) \| gesture: gesture(left-pinch\|left-grab\|left-poke\|right-pinch\|right-grab\|right-poke)) | Android XR: turn (radians) or move (metres) the head, recenter, set the room and passthrough, the input mode, aim or pinch the hand ray and look or select with gaze at pixels (stock's gRPC hand and eye events), a hand gesture, or point the dual-mode mouse |
| `touchpad` | Android | device, x, y, phase?(down\|move\|up\|tap) \| x, y, to_x, to_y, duration_ms? (a swipe) | Touch or swipe the touchpad beside the screen, in touchpad units (Android XR display glasses: `hw.touchpad0`) |
| `stylus` | Android, iOS | device, x, y, phase?(tap\|hover\|down\|move\|up\|leave), pressure?(1-255), button?, eraser? \| x, y, to_x, to_y, duration_ms? (a stroke) | A stylus on the touch screen at pixels. Android: what the stock emulator's gRPC pen events send: hover over it, touch, move, lift or leave, with the barrel button or the eraser end; or draw a stroke. iPad: the Apple Pencil (UITouch type .pencil): tap (hold_ms?), down, move, up or a stroke; hover, leave, pressure, button and eraser are refused with the reason (the simulator's touch screen has none) |
| `rotary` | Android, iOS | device, ticks \| delta (Digital Crown units; iOS) | Turn a watch's rotary input: the Wear OS crown or bezel, the Apple Watch Digital Crown |
| `vehicle` | Android | device, property?(VHAL name or id: GEAR_SELECTION, gear, speed, 0x11600207, ...), area_id?, value? | Android Automotive's vehicle HAL: list every property's value, read one, or set one when `value` is given (typed by the property: drive, 12.5, true); through the emulator console's `car` command (simee) |
| `watch_gesture` | iOS | device, gesture(double-tap\|wrist-flick) | An Apple Watch hand gesture |
| `remote_swipe` | iOS | device, direction(up\|down\|left\|right), duration_ms? | Swipe on the Siri Remote's touch surface (Apple TV) |
| `pointer` | iOS | device, x, y, action?(move\|click\|down\|up), button?(primary\|secondary), hold_ms? \| x, y, to_x, to_y, button? (a drag) | The iPad's pointer (a mouse, as Simulator.app's pointer capture connects): move it to pixels and hover there, click, press or release a button there, or drag to to_x, to_y. Exact on iOS 26.5, within ~20 pt on 27.2. A button held by `down` keeps the pointer's place for the next calls on a server (`simee serve`) until a reboot. Refused: `down` without a server (the process ends with the call: use `click` or a drag), `up` with nothing held, and `move`/`up` after a failed call lost the held button (`click`, `down` or a drag lets go and starts over) |
| `carplay` | iOS | device, on | Attach (or detach) the CarPlay display to a booted iPhone (iOS 26 runtimes); then tap/swipe/screenshot with display=carplay |

## Admission

`boot`, `boot_many`, `create` and `clone` say what the new devices would do to the host before they
start (#275), from `simee-host`'s `admission`. `admission_check` asks the same question on its own (here on a 32 GiB host with 42% of its memory in use):

```text
→ {"id": 1, "method": "admission_check", "params": {"platform": "android", "count": 4}}
← {"id": 1, "ok": true, "result": {"verdict": "hold", "reasons": ["memory: 4 more Android phone devices would take memory used
   from 42% to 86% (3.8 GB each while booting), at or above the hold limit 85%"], "predicted": {"devices": 4, "admissible": 3, ...}}}
```

- **Cost per device**, by platform and form factor: CPU (percent of one core) and memory while it
  boots and once it is idle. Until this process has seen idle devices of the kind (three
  observations at least), the numbers are docs/benchmarks.md's: Android (emu, API 36) boots at 250% of a core and 3764 MB and idles at 5.9%
  and 2.8 GB; iOS (iPhone 17, default profile) boots at 280% and idles at 10% (the settle limit,
  not a measurement), 2176 MB either way. Only phones are measured: other form factors take their
  platform's phone numbers, and `source` says so. A device under 50% of a core over a minute in
  `devices_usage` refines its kind's idle CPU and memory: a rolling average that starts at the
  published numbers and moves a fifth of the way to each observation (kept in memory), so one
  slim device doesn't replace them; booting memory keeps the measured ratio to idle. A server (`simee serve`) learns across calls; a CLI
  call runs alone, so it uses the published numbers.
- **The verdict** is the worse of the `host` verdict now (thermal, power, memory pressure, load)
  and the prediction: hold when memory used would reach 85%, available memory (Linux) would drop
  under 8 GiB, or the 1-minute load plus the booting CPU would reach the load limit (1.5 per CPU);
  back off at 95% used or under 4 GiB. `predicted` has the numbers (`memory_used_pct`,
  `memory_available_bytes`, `load` now and after), each kind's cost and `admissible`, how many
  of the devices (in order) the host takes now. A device that is already booted costs nothing.
  At most `concurrency` (`boot_many`'s, `-j`) devices boot at once, so the prediction counts that
  many at their booting cost and the rest at their idle cost, not every device booting together
  (`admission_check` has no concurrency: all `count` boot together, the worst case).
- **`admission`**: `ignore` (the default) only reports it; nothing waits on it. `refuse` fails
  fast, doing nothing, when the verdict isn't admit, with every reason. `wait` (`boot`,
  `boot_many`) waits until the host admits the devices, asking every 5 s, for at most
  `admission_timeout_s` (600): `boot_many` starts as many as are admissible at a time, each chunk
  booted as `boot_many` boots, and returns `pacing` (`waited_s`, `timed_out`, each start and each
  wait with its reason); devices not admitted in time are not started and fail with the reason.
  `admission_timeout_s` is time spent waiting for the host: the time the started chunks take to
  boot is not counted, and `waited_s` and the error say how long it actually waited. A chunk
  that fails as a whole (no ports left, a plan error) fails its own devices and leaves the chunks
  booted before it in the result. Each wait is a line on stderr and, over MCP, the task's status
  message or the progress message, with the reason; a cancelled call stops waiting within a
  second and boots nothing more (`pacing.cancelled`, the devices not started say so).
- **Known limits.** Callers don't reserve: each judges the host as it is, so several callers
  admitted at the same moment (parallel `simee boot` processes, several agents) can together go
  past a limit (#275). A CLI boot runs in the caller, so it always uses the published numbers
  and has no `--admission` flag; its text output doesn't show the verdict, `--json` and the API
  do.

## Memory tools (`mem_*`)

A simulator app is a host process (a child of its device's `launchd_sim`), so the host's own
`leaks`, `heap`, `vmmap` and `malloc_history` read it (`crates/sim-api/src/memtools.rs`). Each
method names the app by `bundle_id` (the running process started under the launchd label
`UIKitApplication:<bundle id>[…]`) or by `pid` (which must be one of the device's processes), runs
the tool with a deadline (`timeout_s`, default 120; past it the tool simee started is killed and
the call fails), and answers its report parsed, with `pid`, `warnings` (anything the tool printed
to stderr while answering) and, with `raw: true`, the report as printed. Android devices answer
that these aren't available there.

- **Stacks.** `launch {bundle_id, malloc_stack_logging: true}` starts the app with
  `MallocStackLogging=1`, which is "lite" mode: stacks of live allocations only, kept in memory.
  `mem_leaks` then gives each leak group its allocation stack (`{address, image, symbol,
  source?}`, innermost frame first, so frame 0 is the allocator and the app's own frame follows
  with its `file:line` when the binary has debug info); without it `stacks` is false and a `note`
  says how to get them. `mem_history` needs it: a process without stack logs is refused with that
  advice, and an address the log doesn't hold (freed, or never allocated) answers no `events`.
- **Leaks.** `leaks --list --noContent`: `leak_count`, `leaked_bytes`, `nodes`, `malloced_bytes`,
  and `leaks`, grouped by type (`type`, `language`, `binary`) and stack, each with `count`,
  `bytes` and its `instances` (`{address, size, zone}`; an address goes to `mem_history`). A plain
  `malloc` without stack logging has no type.
- **What they need.** The host's tools carry `com.apple.system-task-ports.read` and, for a
  simulator target, run the runtime's own copy (the report's `Analysis Tool`). On macOS 26.6
  (SIP disabled, Developer Mode on) an app ad-hoc signed without `get-task-allow`, and the
  device's SpringBoard, were both read as the user, without `sudo`. The tools refuse a process of
  another user (`leaks 1` as a user: `cannot examine process 1 (launchd) because you do not have
  appropriate privileges …`); a call whose tool is refused fails with that line
  (`leaks was refused pid <n>: …`). A host with SIP enabled hasn't
  been measured; a notice a tool prints while still answering comes back in `warnings`.
- **Cost** (2026-11-27, Apple M3 Max, 16 CPUs, macOS 26.6.2, iOS 26.5 iPhone 17 simulator; a small
  test app with 20 leaks, 13 k heap nodes, 1.8 MB malloced, launched with stack logging; median
  of 3 warm-server calls; wall from `/usr/bin/time` on `simee call`, CPU the server's user+sys
  including the tool it reaped, minus an idle server's): `mem_leaks` 2.30 s wall, 2.28 s CPU;
  `mem_heap` 1.75 s, 1.73 s; `mem_vmmap` 0.60 s, 0.58 s; `mem_history` 0.73 s, 0.71 s. The
  reports name a corpse of the app as their target (`Target Type: corpse`), and the app kept
  running across every call; their time grows with its heap (the same `leaks` command line on
  the device's SpringBoard, run directly: 4.9 s wall, one run).

## App preferences (`prefs_*`)

One typed API over Android SharedPreferences and DataStore preferences and iOS user defaults,
with no SDK in the app (`crates/sim-api/src/prefs.rs`). The iOS backend is implemented; the
Android backend is not yet: every `prefs_*` call on an Android device checks its parameters and
then fails with "app preferences (`prefs_*`) are not yet available on android devices".
`defaults_read` and `defaults_write` are unchanged.

**Stores.** `prefs_stores {device, bundle_id}` lists the app's stores as
`{id, kind, name, bundle_id, platform}` (`bundle_id` is the Android package name on Android); the
other methods name a store by its `id`:

| kind | Platform | id | Holds |
| --- | --- | --- | --- |
| `shared_preferences` | Android | `shared_prefs/<name>.xml` | string, int, long, float, bool, string_set |
| `datastore` | Android | `datastore/<name>.preferences_pb` | string, int, long, float, double, bool, string_set, data (DataStore 1.1's `byteArrayPreferencesKey`) |
| `app_domain` | iOS | the bundle id | string, int, long, float, double, bool, date, data, array, dictionary |
| `app_group` | iOS | the suite, `group.…` | as `app_domain` |
| `domain` | iOS | any other domain name | as `app_domain` |

**Values.** Every value travels with its type, `{"type": <type>, "value": <value>}`, so an `int`
comes back an `int` and a `long` a `long`:

```text
{"type": "string", "value": "Ada"}
{"type": "int", "value": 3}                       32-bit
{"type": "long", "value": 9007199254740993}       64-bit
{"type": "float", "value": 0.1}                   32-bit; also "NaN", "Infinity", "-Infinity"
{"type": "double", "value": 2.5}                  64-bit; the same three strings
{"type": "bool", "value": true}
{"type": "string_set", "value": ["a", "b"]}       sorted, each string once
{"type": "date", "value": "2026-11-27T10:00:00.25Z"}   ISO 8601 in (any offset), UTC out, to the nanosecond
{"type": "data", "value": "AAEC"}                 standard base64
{"type": "array", "value": [{"type": "int", "value": 1}]}
{"type": "dictionary", "value": {"k": {"type": "bool", "value": true}}}
{"type": "unsupported", "value": {"native": "integer", "raw": "18446744073709551615"}}
```

An array's elements and a dictionary's values are typed values too, nested to any depth.

- **iOS keeps no width.** A plist has one integer and one real, so an iOS store reads every
  integer as `long` and every real as `double`, whatever wrote it: `int` and `long` write an
  integer, `float` and `double` a real.
- **`unsupported`** is what a store holds that no type here holds exactly: a plist integer past 64
  bits (Swift's `UInt64.max`), a SharedPreferences `<double>` or `<null>` element that another
  tool wrote (Android's own API writes neither). It carries the platform's name for the type
  (`native`) and the text as stored (`raw`). It is read-only: a write or batch op naming it is
  refused.

`prefs_read {device, bundle_id, store?}` answers `[{store, values: {<key>: <typed value>}}]`. With
`store`, an unreadable store fails the call. Without it, every store of the app answers on its own,
`{store, values}` or `{store, error}`, so one unreadable store doesn't hide the others. On iOS a read decodes the store's plist on the host (well under a millisecond, no spawn); cfprefsd writes an app's own changes to disk about every 10 s, so `fresh: true` asks cfprefsd instead (one in-device call, about 180 ms).

**Writes.** `prefs_write {device, bundle_id, store, key, type, value}` and `prefs_delete {device,
bundle_id, store, key}` answer `{store, key, visible, app, note?}`. A write is refused before
anything is written when the value isn't exactly of its type (a fraction for an `int`, a number
past its range, a string for a `bool`, an untyped array element) or the store can't hold the type
(a `double` in SharedPreferences, a `string_set` on iOS); errors name the value (`value[2].value`,
`ops[1].value`).

- `visible`: `now` when the app sees the write at once (a running app it reached, or an app that
  wasn't running, or was stopped for the write, and reads it when it starts), or `after_relaunch`
  when a running app sees it only once relaunched.
- `app`: what was done to the app for the write: `untouched` (it wasn't running, or the write
  reached it where it runs), `stopped` (it was running and was stopped for the write; it stays
  stopped), or `relaunched` (stopped, written, launched again).
- `note` says why when the app was stopped or sees it only after a relaunch.

On Android a running app keeps its preferences in memory and writes the whole copy back on its
next `apply`/`commit` (SharedPreferences) or update (single-process DataStore), over a write made
under it. So an Android write can't promise `after_relaunch` to a running app: it stops the app,
writes, and says whether it relaunched it.

**Batches.** `prefs_batch {device, bundle_id, store, ops}` takes `{key, type, value}` sets and
`{key, delete: true}` deletes and is all or nothing: every op is checked first (each key at most
once), and the backend then writes the store once with all of them, or fails with none applied.
On iOS, if cfprefsd's final synchronize fails after the keys were handed to it, the error names the
keys sent and says some may already be live in the running app.
It answers `{store, keys, visible, app, note?}`.

**Watching.** `prefs_watch {device, bundle_id, store?, cursor?, wait_ms?}` works like `logs`'
`stream`: the first call (no `cursor`) starts a feed of the app's stores (or one) in the process
that answers (a `simee serve` keeps it between commands) and returns `{cursor, events: []}`. A call
with that cursor returns the changes since, oldest first, and the next cursor:

```text
→ {"method": "prefs_watch", "params": {"device": "pixel", "bundle_id": "com.example", "cursor": "3:0", "wait_ms": 30000}}
← {"id":null,"ok":true,"result":{"cursor":"3:2","events":[
    {"store":"shared_prefs/com.example_preferences.xml","key":"launches","old":{"type":"int","value":3},"new":{"type":"int","value":4},"at":"2026-11-27T10:00:01Z"},
    {"store":"shared_prefs/com.example_preferences.xml","key":"token","old":{"type":"string","value":"x"},"new":null,"at":"2026-11-27T10:00:02Z"}]}}
```

- `old` is `null` for a key that was added, `new` for one that was removed; a write that changes
  nothing reports nothing. Changes come from whoever made them: the app, these methods, another
  tool.
- A feed's own thread takes changes as the backend sends them, so a call returns what has
  arrived. With nothing new, it waits up to `wait_ms` (at most 60000) for a change. Callers
  sharing a feed don't hold each other up: one waiting doesn't delay another's read.
- `missed: <n>`: the feed keeps the latest 16 MiB of changes (by their values' sizes), and `n`
  older ones went unread.
- `reset: true`: the cursor isn't this feed's (it stopped and another started); the new cursor
  starts now, and changes in between may be lost, so read the store again.
- `ended: true`: the feed has ended, because the device stopped or nobody read it for 5 minutes
  (it then drops what it kept). A call with its cursor still gets what it had; a call without a
  cursor starts a new feed.
- To follow a store from a known state: `prefs_watch` first, then `prefs_read`, then poll with the
  cursor; a change made during the read is in the feed.

## Streams (`stream_*`)

`stream_start` publishes a device's screen as H.264 over WebRTC to a WHIP endpoint (a media
server such as MediaMTX, which browsers then play over WHEP). It returns once the first frame is
encoded and the WHIP session is connected (the shape below; the values are placeholders):

```text
→ {"method": "stream_start", "params": {"device": "iPhone 17", "whip_url": "http://127.0.0.1:8889/sim/whip"}}
← {"id":null,"ok":true,"result":{"stream_id":"stream-1","whip_resource":"http://127.0.0.1:8889/sim/whip/1f0c…","lease_ttl_s":30,"device":"5B0E…"}}
→ {"method": "stream_status", "params": {"stream_id": "stream-1"}}
← {"id":null,"ok":true,"result":{"stream_id":"stream-1","state":"running","stages":[{"stage":"requested","ms":0.0},
    {"stage":"first_encoded_frame","ms":…},{"stage":"whip_connected","ms":…}],"stats":{"fps":…,"keyframes":…,"pli":…,…},…}}
```

- **`input: true` needs a server that carries data channels.** MediaMTX refuses the offer (`406
  unsupported media 'application'`, docs/streaming.md "Input over a data channel"), so the
  `stream_start` fails; leave `input` off for a MediaMTX endpoint.
- A stream lives in the process that started it: run `simee serve` (the CLI forwards to it) or
  the MCP server. One stream per device and WHIP URL; a second is refused with the first's id.
- **Leases.** `stream_status` renews a stream's 30 s lease; a stream nobody renews is stopped
  (its WHIP resource deleted), so a client that crashed leaves nothing running. `streams` lists
  without renewing. A stream that ended on its own (the peer disconnected, the device went away)
  stays listed, `state: "ended"` with its `reason`, until its lease lapses.
- **Stopping.** `stream_stop` ends the encode loop, sends the WHIP DELETE and returns the last
  status with `deleted`.
- **Errors.** A URL the publisher refuses (`https`, plaintext to a host that isn't loopback, not
  http), a device that isn't booted, the WHIP server refusing the token (401/403), `audio: true`
  (#265), and a build without an encoder (a Linux `simee` built without `--features openh264`, or an iOS device on a Linux host) fail `stream_start`, leaving nothing
  running. The token is never echoed.
- **Input.** `input: true` offers the `simee-input` data channel; its events reach the device's
  `touch`, `key`, `type`, `button`, `rotate`, `fold` and `swipe`, mapped from the stream frame to
  device pixels. docs/streaming.md, "Streams from the API", has the mapping and the limits.
