//! The method table and dispatch. A `device` parameter is resolved across both platforms (or one,
//! with `platform`), then the method runs on that device's backend: `ios.rs` (macOS hosts only) or
//! `android.rs`. A method a platform doesn't offer fails with the platforms it does work on.

use std::time::Duration;

use serde_json::{Value, json};
use sim_core::device::{DeviceInfo, DeviceState, DeviceTypeInfo, RuntimeInfo, Selection, select};
use sim_core::form_factor::{FormFactor, Kind, matrix};
use sim_core::platform::Platform;
use sim_host::Verdict;
use sim_host::admission::{Admission, Clock, HostState, Item, Limits, Paced, Step, SystemClock, Wait, admit, pace, verdict_name};

use crate::params::{need_str, opt_form_factor, opt_platform, str_param};
use crate::progress::{BootHook, Handoff, call_cancelled, call_progress, call_step, has_call_hooks, interruptible, interruptible_watching};

/// (method, parameters, description). `device` is a UDID, a name, or `booted`. [`platforms`]
/// says which platforms' devices each method works on.
pub const METHODS: &[(&str, &str, &str)] = &[
    ("help", "", "List methods with the platforms each supports"),
    (
        "identity",
        "",
        "This process's build, executable and environment (hashed): the CLI forwards a command to a server only when they match its own",
    ),
    (
        "host",
        "",
        "Host health, sampled without forking: load averages and CPU count, memory pressure (macOS: the kernel's level; Linux: available \
         memory and PSI), swap, free disk on the data volume, thermal state (macOS: the system thermal state; Linux: thermal zones, \
         frequency caps, throttle events), power (source, battery, Low Power Mode or platform profile), 1/5/15-minute averages from a \
         1 s ring (with time spent in each thermal state), the load trend, and an admit/hold/backoff verdict with every reason",
    ),
    (
        "devices_usage",
        "iOS: set?",
        "Which running devices load the host, sampled without forking: per device (iOS simulator: its launchd_sim tree; Android: \
         simee's emu or the stock emulator's qemu, with their children) id, name, platform, form factor, CPU % of a core over 1 s and \
         1 min, resident memory and footprint, disk read/write bytes/s, energy (W) where the kernel reports it, and process count, \
         costliest first; simee's own processes; and host totals (busy CPU, the devices' and simee's share of it)",
    ),
    (
        "form_factors",
        "platform?, form_factor?",
        "Every form factor on every platform: sub-platform, inputs and reference profile, or why it's unsupported",
    ),
    (
        "devices",
        "booted?, platform?(android|ios), form_factor?; iOS: set?",
        "List devices with their platform, form factor and sub-platform",
    ),
    ("runtimes", "platform?; iOS: set?", "List installed runtimes (Android: system images)"),
    (
        "device_types",
        "platform?; iOS: set?",
        "List device types. Android: the hardware a new AVD follows (phone and tablet, the foldables pixel_fold and surface_duo, \
         and the SDK definitions for Wear OS, TV, Automotive and Android XR's headset and glasses), each with its screen, the scale \
         its dpi over 160; `create` takes its form factor, and a foldable's identifier as `device`",
    ),
    (
        "create",
        "name, platform?(ios|android), form_factor?, admission?(ignore|refuse); iOS: device_type, runtime, set?; Android: system_image, device?(pixel_fold|surface_duo)",
        "Create a device. form_factor is checked against the device type (iOS) or the system image (Android) and shapes a new AVD. \
         Returns the device with `admission`, what booting it would do to the host (as `admission_check`); admission refuse fails \
         fast, creating nothing, unless the verdict is admit",
    ),
    ("delete", "device; iOS: set?", "Delete a device"),
    (
        "boot",
        "device, timeout_s?, admission?(ignore|refuse|wait), admission_timeout_s?; iOS: env?, shaped?, settle?, settle_cpu_pct?, settle_window_s?, \
         settle_foreground?(true), settle_timeout_s?, profile?(\"slim\"), set?; Android: audio?(false), camera_back?, camera_front?",
        "Boot and wait until finished (iOS: env is the device launchd's environment, shaped injects the network shaper; settle also waits \
         up to settle_timeout_s (600) until the device's processes use under settle_cpu_pct (10) % of a core for settle_window_s (10) s \
         (settle_foreground, the default: counting only foreground-QoS CPU) and returns the timings with cpu_pct, foreground_cpu_pct, \
         background_cpu_pct and cpu_s (CPU-seconds the tree used since the boot; for a device already booted, since the first sample), settled false if it ran out of time; profile turns off a named set \
         of daemons (written before the boot of a shut-down device, so they never start); Android: starts emu and \
         waits for sys.boot_completed, silent unless audio, which plays the device on the host's default output; camera_back and \
         camera_front link a camera source first, as camera_link does, saved for later boots too). Returns `admission`, \
         what the device would do to the host (as `admission_check`; nothing for one already booted): admission ignore, the default, \
         only reports it; refuse fails fast unless the verdict is admit; wait waits until it is (at most admission_timeout_s, 600) \
         and returns `pacing`",
    ),
    (
        "boot_many",
        "devices, concurrency?, timeout_s?, admission?(ignore|refuse|wait), admission_timeout_s?; iOS: env?, shaped?, settle?(default true), \
         settle_cpu_pct?, settle_window_s?, settle_foreground?(true), settle_timeout_s?, profile?(\"slim\"), set?; Android: first_port?(5554)",
        "Boot several devices of one platform, at most `concurrency` at once (default: a quarter of the host's CPUs, at least 2), each as \
         `boot` does (iOS: a slot is held until the device settles, unless settle is false); returns each device's timings or error, \
         failed and unsettled counts, and the time until all were done. Android: each on its own console port from first_port up. \
         Returns `admission` for the devices not running yet (as `admission_check`): admission ignore, the default, only reports it; \
         refuse fails fast unless the verdict is admit; wait paces the boots, starting as many as the host admits and asking again \
         every 5 s, and returns `pacing` (each start and wait); devices not admitted within admission_timeout_s (600) are not \
         started and fail with the reason",
    ),
    (
        "admission_check",
        "platform(ios|android), form_factor?(phone), count?(1)",
        "Whether the host can take `count` more devices of a platform and form factor now: each one's predicted cost while booting \
         and idle (CPU % of a core and memory: docs/benchmarks.md's numbers until this process has seen idle devices of the kind \
         running, then this host's), what they would do to memory used and the 1-minute load, and an admit/hold/backoff verdict \
         (the `host` verdict, which still applies, and the prediction: hold at 85% memory used or the host's load limit, back off \
         at 95%) with every reason and how many of them are admissible now. `boot`, `boot_many`, `create` and `clone` return the \
         same as `admission`",
    ),
    (
        "settle",
        "device, cpu_pct?, window_s?, foreground?(true), timeout_s?(600); iOS: set?",
        "Wait until a booted device is quiet: its processes under cpu_pct (10) % of a core for window_s (10) s (foreground, the default: \
         only foreground-QoS CPU counts), SpringBoard running; returns settle_s, cpu_pct, foreground_cpu_pct and background_cpu_pct, and cpu_s and background_cpu_s (CPU-seconds used since the first sample). \
         Android: the guest's processes and kernel threads from /proc over adb, once a second on the guest's clock (the probe's own \
         shell left out), all of it foreground (no QoS split: background_cpu_pct is 0), with sys.boot_completed and system_server \
         running in place of SpringBoard",
    ),
    (
        "profile",
        "device, name(slim), undo?; iOS: set?",
        "Turn a named set of daemons off on a booted device (iOS: disabled in its launchd database and stopped; Android: packages \
         disabled for user 0, `pm disable-user`; both stay off across reboots), or with undo back on (iOS: from the next boot; \
         Android: at once); returns how many changed, those that failed and those that weren't loaded (Android: not on this image). \
         Android's slim: Device Health Services, Digital Wellbeing, Data Restore Tool and Google Partner Setup (savings not yet measured)",
    ),
    (
        "clone",
        "device, name, admission?(ignore|refuse); iOS: set?",
        "Create a device as a copy of a shut-down one. iOS: a simulator (data, settings, apps; APFS clones of its files); Android: an AVD \
         from a golden that has booted under simee (copy-on-write clones of its disks). A booted-once template's copies skip the \
         first-boot work. Returns the copy with `admission`, as `create` does",
    ),
    (
        "export",
        "device, to",
        "Pack a shut-down device into a portable bundle at `to` (a .tar file, else a new directory) with no host path in it: \
         manifest.json and content-addressed chunks (1 MiB, BLAKE3, each LZ4-compressed; zero ranges stored as extents). \
         Android: an AVD that has booted under simee (its simee disks, config.ini without host paths, boot.prop) and \
         its system image's identity; captures disks, not RAM (resume is a cold boot). Returns sizes, the identity and the \
         secrets inventory. iOS simulators: not yet (the next step of #170)",
    ),
    (
        "import",
        "from, name, allow_mismatch?(false)",
        "Create device `name` from a bundle `export` wrote. Android: finds the system image by content (API, tag, ABI, revision, \
         BLAKE3 of its images, fingerprint) and refuses, naming what to install, when none matches (allow_mismatch: use another \
         installed revision of the same API, tag and ABI, named in `mismatch`); checks every chunk and disk hash; drops \
         config.ini keys holding host paths (listed in `dropped`); writes nothing unless it all succeeds. The import keeps the \
         exported device's guest identity (ro.serialno, android_id). iOS: not yet",
    ),
    ("shutdown", "device; iOS: set?", "Shut down"),
    ("erase", "device, set?", "Erase content and settings (shut down first)"),
    (
        "screen",
        "device, display?(main|outer|inner|carplay); iOS: set?",
        "Screen size in pixels, the space tap, swipe and touch take (a folded Android foldable: the size its guest lays out for, a Pixel Fold's 1080x2092, as uiautomator's bounds). \
         display (iOS): main is the screen in use (an unfolded iPhone Duo's inner one), outer and inner an iPhone Duo's two, carplay an iPhone's CarPlay screen",
    ),
    (
        "screenshot",
        "device, path?, display?(main|outer|inner|carplay); iOS: set?",
        "PNG to path, else base64 in `png` (a folded Android foldable: its outer screen as the scanout shows it, a Pixel Fold's 949x1840; \
         an iPhone Duo's inner screen as its framebuffer holds it, 2007x2853)",
    ),
    (
        "wait_idle",
        "device, quiet_ms?(300), timeout_ms?(10000), screenshot?(false), path?, display?(main|outer|inner|carplay), set?",
        "Wait until the screen is still: no frame has changed it for quiet_ms, at most timeout_ms (fails if it kept changing). \
         Follows the display's frame callback and surface seed, reading no pixels while nothing is presented; where the seed \
         ticks on a still screen (iOS 26.5, 10 times a second) a content hash decides. Returns waited_ms and frames (seed \
         changes seen), and with screenshot the still screen as screenshot returns it (path, else base64 in `png`)",
    ),
    (
        "stream_start",
        "device, whip_url, token?, fps?(30), max_size?(1280), bitrate?(2500000), audio?(false), input?(false); iOS: set?",
        "Stream the screen as H.264 over WebRTC to a WHIP endpoint (plaintext http to a loopback host only, e.g. MediaMTX's \
         `http://127.0.0.1:8889/<path>/whip`), with token as its bearer token; max_size caps the longest side (0: full size), \
         bitrate is in bit/s; input offers the `simee-input` data channel, whose touches, keys, text, buttons, rotations, folds \
         and scrolls reach the device (a server that refuses the data channel, as MediaMTX does, gets the stream without it: the result's input is false and input_note says why). One stream per device and URL, living in this process (`simee serve`) on a 30 s lease \
         that stream_status renews: a stream nobody renews is stopped. Returns stream_id, whip_resource and lease_ttl_s (and, with input, whether it is on). \
         Encoded on macOS by VideoToolbox (iOS and Android devices) and on Linux by OpenH264 in a build with `--features openh264` (Android devices only); audio isn't streamed yet",
    ),
    (
        "stream_status",
        "stream_id",
        "A stream's state (starting, running, ended and why), its stages in the order reached (requested, first_encoded_frame, \
         whip_connected, ended; ms since requested) and stats (encoded, sent, keyframes, bytes, fps, bitrate_bps, the publisher's \
         bitrate_estimate_bps, the viewers' pli and fir keyframe requests, input events and errors); renews its lease",
    ),
    ("stream_stop", "stream_id", "Stop a stream: ends its encode loop and sends the WHIP DELETE; its last status, with deleted"),
    ("streams", "device?; iOS: set?", "This process's streams (or one device's), each as stream_status shows it, without renewing a lease"),
    (
        "contrast_check",
        "device, region?, scale?, appearance?(current|both), restore?(light|dark), settle_ms?(500), path?, display?(main|outer|inner|carplay); iOS: set?",
        "Text and controls below WCAG contrast in a screenshot, from pixels alone: finds text-like regions, estimates each one's foreground and \
         background, and reports those under 4.5:1 (normal text) or 3:1 (large text of about 18 pt, icons and controls) with their box, the \
         two colours and the ratio, and the screenshot with the findings boxed (`png`, or written to path). Text over a photo or a busy \
         background has no one background and isn't judged. appearance both (iOS) checks the light and the dark UI (switches, waits settle_ms, \
         checks) and leaves the device in restore (default light); Android can't switch appearance yet. region is {x, y, width, height} \
         in pixels; scale is pixels per point (default: the device's, else 1)",
    ),
    (
        "tap",
        "device, x, y, hold_ms?, display?(main|outer|inner|carplay); iOS: set?",
        "Tap at pixels, held hold_ms (default: iOS 10, Android 60) (display, iOS only: an iPhone Duo's outer or inner screen, an \
         iPhone's CarPlay screen)",
    ),
    (
        "swipe",
        "device, x1, y1, x2, y2, duration_ms?, settle?, display?(main|outer|inner|carplay), edge?(top|bottom|left|right; iOS); iOS: set?",
        "Swipe between pixels over duration_ms (default 300; iOS sends its samples on schedule, so a busy host doesn't stretch \
         it); with edge (iOS) the touch comes in from that screen edge, as a drag from Simulator.app's bezel: \
         home and the app switcher from the bottom, Notification Center and Control Center from the top (Android's edge \
         gestures need only the start point)",
    ),
    (
        "touch",
        "device, x, y, phase(down|move|up), display?(main|outer|inner|carplay), edge?(top|bottom|left|right; iOS); iOS: set?",
        "One raw touch sample",
    ),
    (
        "button",
        "device, button(home|lock|side|siri|apple-pay|action | back|volume-up|volume-down|recents|menu|up|down|left|right|select|play-pause), hold_ms?; iOS: set?",
        "Press a hardware button, as the device's form factor has it: the Siri Remote on Apple TV (menu/back, home, select, \
         play-pause, arrows) and the D-pad on Android TV; on Apple Watch home is the crown's press. Held hold_ms (default: 10 \
         for home and lock on an iPhone or iPad, else 80)",
    ),
    ("type", "device, text; iOS: set?", "Type on the hardware keyboard (iOS: a key report every 3 ms)"),
    ("key", "device, usage; iOS: set?", "Press a key by HID usage (iOS: down and up 3 ms apart)"),
    ("open_url", "device, url; iOS: set?", "Open a URL (Android: `am start -a VIEW`)"),
    (
        "install",
        "device, path; iOS: set?",
        "Install an .app (Android: an .apk with `adb install -r`, an .aab through bundletool when it is on the PATH)",
    ),
    ("uninstall", "device, bundle_id; iOS: set?", "Uninstall an app (Android: the package name; `pm uninstall`)"),
    (
        "launch",
        "device, bundle_id, restart?; iOS: args?, env?, shaped?, malloc_stack_logging?, set?",
        "Launch (iOS: with arguments and environment, shaped injecting the network shaper, malloc_stack_logging setting \
         MallocStackLogging=1 for mem_leaks' stacks and mem_history; Android: the package's launcher activity); \
         restart: terminate a running instance first, default true (Android: `am start -S`); false brings a running app to the \
         front; returns pid (Android: and the activity it started)",
    ),
    ("terminate", "device, bundle_id; iOS: set?", "Terminate an app (Android: `am force-stop`)"),
    ("apps", "device; iOS: set?; Android: all?", "Installed bundle ids (Android: the third-party packages, with all the system's too)"),
    ("appearance", "device, mode(light|dark), set?", "Light or dark UI"),
    ("location", "device, latitude, longitude; iOS: set?", "Simulated location"),
    ("privacy", "device, action(grant|revoke), service, bundle_id, set?", "Privacy permission"),
    ("push", "device, bundle_id, payload, set?", "Deliver an APNs payload (object)"),
    (
        "status_bar",
        "device, time?, data_network?, wifi_mode?, wifi_bars?, cellular_mode?, cellular_bars?, operator?, battery_state?, battery_level?, set?",
        "Override status bar",
    ),
    ("clear_status_bar", "device, set?", "Remove status bar overrides"),
    ("content_size", "device, size, set?", "Dynamic Type size (name or 0-11)"),
    ("increase_contrast", "device, on, set?", "Increase Contrast"),
    ("dynamic_island", "device, suppressed, set?", "Hide the Dynamic Island"),
    ("backlight", "device, on, set?", "Display backlight"),
    ("keyboard_language", "device, language, set?", "Software keyboard language"),
    ("hardware_keyboard", "device, on, set?", "Connect the hardware keyboard"),
    ("ca_debug", "device, option, on, set?", "Render overlay (see ca_debug_state for names)"),
    ("ca_debug_state", "device, set?", "Every render overlay and whether it is on"),
    ("reset_keychain", "device, set?", "Delete every keychain item"),
    ("memory_warning", "device, set?", "Simulate a memory warning"),
    ("notify", "device, name, state?, set?", "Post a Darwin notification (optionally setting its state)"),
    ("biometric", "device, action(match|nomatch|enroll|unenroll), face?, set?", "Touch ID / Face ID"),
    (
        "exec",
        "device, program, args?, env?; iOS: set?; Android: timeout_s?, max_bytes?",
        "Run a program inside the device; returns stdout/stderr. Android: over adb as the shell user, the argv passed through \
         unchanged (quoted, so nothing in it is expanded; a program can't start with `-`), with exit_code; past timeout_s (30, at \
         most 86400) it is stopped and the call fails, and a stream that reaches max_bytes (8 MiB) is cut there (truncated true) \
         and the program stopped if it still ran (exit_code null). Stopping it stops its process group, so what it started too",
    ),
    (
        "logs",
        "device, last?, predicate?, stream?(false), set?",
        "Unified log lines (default last 1m), as `log show --style compact` prints them. stream: the first call starts one in-device \
         `log stream` for the device's boot, and later calls answer the window from it without spawning (log show answers a window \
         reaching back before the stream started, a predicate, and a `last` other than <n>s|m|h|d); the stream stops after 5 idle \
         minutes or a reboot. It omits launchd_sim's lines, signpost state dumps and `Last log repeated` lines (iOS 27.2: 12% of a \
         settled device's lines, 1% of a just-booted one's), and its timestamps are often 1 ms later",
    ),
    (
        "processes",
        "device; iOS: set?",
        "Device processes with CPU and memory. Android: the guest's `ps -A` (cpu is CPU time over the process's life), each app \
         process with its package",
    ),
    (
        "kill",
        "device, pid | bundle_id, signal?; iOS: set?; Android: force?",
        "Signal a device process (default 15, SIGTERM). iOS takes pid only. Android: bundle_id stops the app instead (`am \
         force-stop`); a process the shell user may not signal is signalled as root (`su 0`, on google_apis images); init, zygote, \
         zygote64, system_server, surfaceflinger, servicemanager, hwservicemanager, vndservicemanager, vold, lmkd, logd, adbd, netd, \
         installd, ueventd, keystore2 and statsd are refused unless force",
    ),
    (
        "connections",
        "device; iOS: set?",
        "Open sockets of device processes (Android: the guest kernel's /proc/net/tcp, tcp6, udp and udp6, matched to processes by \
         the socket inodes in /proc/<pid>/fd, as root where the image has su; each row has the socket's uid, and pid is null for \
         one no process could be matched to, as another user's is without root)",
    ),
    (
        "traffic",
        "device; iOS: set?",
        "Bytes in/out per device process (Android: per app uid since boot, from the kernel's eBPF mAppUidStatsMap as dumpsys \
         prints it, or /proc/net/xt_qtaguid/stats on Android 9 and older; each row has the uid, and pid is the app's own \
         process, null when it isn't running or the uid is shared)",
    ),
    // Memory tools on one app (#183): the host's leaks, heap, vmmap and malloc_history on its pid.
    (
        "mem_leaks",
        "device, bundle_id | pid, raw?, timeout_s?(120), set?",
        "Leaked memory of a running app (the host's `leaks`): totals, and the leaks grouped by type and allocation stack, \
         most bytes first, each with its instances and stack (innermost frame first; launch with malloc_stack_logging for \
         stacks). raw adds the report as printed",
    ),
    (
        "mem_heap",
        "device, bundle_id | pid, raw?, timeout_s?(120), set?",
        "A running app's heap by class (the host's `heap`): zones, node and byte totals, and every class with its count, \
         bytes and average size, most instances first. raw adds the report as printed",
    ),
    (
        "mem_vmmap",
        "device, bundle_id | pid, raw?, timeout_s?(120), set?",
        "A running app's memory regions (the host's `vmmap --summary`): each region type's virtual, resident, dirty, \
         swapped, volatile and empty bytes and count, the malloc zones with allocations and fragmentation, and the \
         totals. raw adds the report as printed",
    ),
    (
        "mem_history",
        "device, bundle_id | pid, address, raw?, timeout_s?(120), set?",
        "The allocation stack of an address in a running app (the host's `malloc_history`), as events with their stacks; \
         needs the app launched with malloc_stack_logging (MallocStackLogging=1 logs live allocations only). raw adds \
         the report as printed",
    ),
    (
        "containers",
        "device, sizes?; iOS: set?; Android: root?",
        "App data and bundle containers. Android: each third-party app's data dir (`data/user/0/<package>`) and APK dir; the \
         file methods take paths in one app's container (`data/data/<package>`, `data/user/<n>/<package>`, \
         `sdcard/Android/data/<package>`, the APK dir read only) and reach it through run-as for a debuggable app, otherwise \
         as root when adbd is root or `su 0` works. None of them runs `adb root` unless given root: true, since it restarts \
         adbd and drops the device's adb forwards and open shells",
    ),
    ("ls", "device, path?; iOS: set?; Android: root?", "List a directory under the device data dir (Android: in an app's container)"),
    ("read", "device, path; iOS: set?; Android: root?", "Read a file (base64; Android: at most 128 MiB)"),
    ("write", "device, path, text | base64; iOS: set?; Android: root?", "Write a file"),
    ("rm", "device, path; iOS: set?; Android: root?", "Remove a file or directory"),
    ("du", "device, path?; iOS: set?; Android: root?", "Size in bytes (Android: of a path in an app's container)"),
    (
        "clear_container",
        "device, path; iOS: set?; Android: root?, pm_clear?",
        "Empty a container (an app's data) without deleting it (Android: or a directory in one. pm_clear: true clears an app's \
         whole data dir with `pm clear --user <n>` instead, which reaches a release app without root but also stops the app, \
         wipes its external dir and resets its runtime permissions)",
    ),
    ("defaults_read", "device, domain, set?", "Read a preferences domain"),
    ("defaults_write", "device, domain, key, value, type?, set?", "Write a preference (type: string|int|float|bool)"),
    (
        "prefs_stores",
        "device, bundle_id; iOS: set?",
        "An app's preference stores, each with the id the other prefs_* methods take: Android `shared_prefs/<name>.xml` and DataStore \
         `datastore/<name>.preferences_pb`, iOS the app's domain (its bundle id) and its app-group suites; any other iOS domain is \
         reachable by its name",
    ),
    (
        "prefs_read",
        "device, bundle_id, store?, fresh?; iOS: set?",
        "Every key of an app's preference stores (or one) with its typed value, {type, value}: string, int, long, float, double, \
         bool, string_set (Android), date (ISO 8601), data (base64), array and dictionary (iOS; of typed values), and the \
         read-only unsupported {native, raw} for what no type holds exactly. iOS reads integers as long and reals as double. \
         Without store, each store answers {store, values} or {store, error}. fresh (iOS): read through cfprefsd, one \
         in-device call, for writes it hasn't flushed to disk yet (it flushes about every 10 s)",
    ),
    (
        "prefs_write",
        "device, bundle_id, store, key, value, type(string|int|long|float|double|bool|string_set|date|data|array|dictionary); iOS: set?",
        "Set one key to a typed value (refused when the value isn't of its type or the store can't hold the type); visible says \
         whether the app sees it now or after_relaunch, app what was done to the app for it (untouched, stopped, relaunched: \
         a running Android app writes its old values back over the file), with a note on why",
    ),
    ("prefs_delete", "device, bundle_id, store, key; iOS: set?", "Remove one key; visible and app as prefs_write's"),
    (
        "prefs_batch",
        "device, bundle_id, store, ops([{key, type, value} | {key, delete: true}]); iOS: set?",
        "Set and remove several keys of one store, all or nothing: every op is checked first (each key once), then the store \
         is written once or not at all",
    ),
    (
        "prefs_watch",
        "device, bundle_id, store?, cursor?, wait_ms?; iOS: set?",
        "Changes to an app's preference stores (or one), as `logs` stream does: the first call starts a feed in this process and \
         returns a cursor; a call with it returns the changes since, {store, key, old, new, at}, oldest first, waiting up to \
         wait_ms (at most 60000) for one, and the next cursor (reset: the feed restarted, re-read; missed: changes dropped unread \
         past its 16 MiB; ended: the device stopped or nobody read it for 5 minutes, and a call without a cursor starts anew)",
    ),
    ("crash_reports", "device, set?", "Host crash reports for this device"),
    ("info", "device; iOS: set?", "Paths and identity"),
    (
        "network_shape",
        "device, preset?(offline|3g|edge|lte|lossy|very-bad|none), offline?, latency_ms?, down_kbps?, up_kbps?; iOS: loss_pct?, port_base?, port_count?, rules?([{pattern, offline?, latency_ms?, down_kbps?, up_kbps?, loss_pct?}]), proxy?(default true), inject?(false|true|\"launchd\"|\"boot\"), set?",
        "Shape the device's network without root. proxy: a shaping HTTP/SOCKS proxy set in the device's configd_sim. inject: an in-process socket shaper; \"boot\" reboots the device with it in launchd's environment so every process, WebKit included, is shaped live by the profile. \
         Android: the emulator's NAT through its console (`network speed <up>:<down>` kbit/s, `network delay <ms>`; what isn't given keeps the device's value, and only what changes is sent) and offline as airplane mode (`cmd connectivity airplane-mode`), returning the commands sent; \
         loss_pct (so lossy and very-bad), rules, port ranges, proxy and inject are refused there, as its console shapes speed and latency only",
    ),
    (
        "network_state",
        "device; iOS: set?",
        "Current profile, proxy port and injection state (Android: the console's `network status`, as network_status, and airplane \
         mode as offline; a latency range set from the console reads as its mean; proxy_port null, injected false)",
    ),
    (
        "network_flows",
        "device, set?",
        "Connections through the device's shaping proxy: host, bytes, duration, outcome (not on Android: the emulator's NAT keeps \
         only its live flows, with no history or byte counts per flow; connections lists the open sockets)",
    ),
    (
        "network_clear",
        "device; iOS: set?",
        "Remove shaping: clear the device proxy, stop the proxy, remove injection (Android: `network speed full`, `network delay none` \
         and airplane mode off)",
    ),
    (
        "audio_state",
        "device; iOS: set?",
        "Device audio settings. iOS: the routed host UIDs and volume, and every host audio device. Android: from `dumpsys audio`, \
         the media stream's volume (as audio_volume's level) and mute, every stream's index, range, mute and output devices, the \
         ringer mode and the audio focus stack with its owner; no host devices (emu plays on the host's default output)",
    ),
    (
        "audio_volume",
        "device, level(0-100); iOS: set?",
        "Device output volume. iOS: 100 = unity, for audio clients started afterwards. Android: the media stream's volume index, \
         the level scaled to its range, live (`cmd media_session volume`, or `media volume` on older images)",
    ),
    (
        "audio_route",
        "device, scope(input|output), uid | \"default\", set?",
        "Pin the device's input or output to a host device (any device, including loopback); applies to clients started afterwards. \
         Android: not available: emu's sound card uses the host's default output and input, with no \
         per-device route, and the guest has one output, the card",
    ),
    (
        "audio_capture_start",
        "device, path?(.wav), mute?, set?",
        "Capture this device's audio output with a process tap (no driver); mute silences it on the host meanwhile; returns capture_id. \
         Android: not available yet: emu's sound card feeds one host sink, the host's output, and has no tap to record from (#265)",
    ),
    (
        "audio_capture_stop",
        "capture_id",
        "Stop a capture: frames, duration_ms, rms, peak, dbfs, path. Android: not available, as audio_capture_start",
    ),
    (
        "audio_mute",
        "device, on; iOS: set?",
        "Silence (or restore) this device's audio; returns whether it is muted. iOS: on the host, a per-device null sink. Android: \
         mutes the guest's media stream (`cmd audio adj-mute`, API 35 and later; earlier images have no command for it)",
    ),
    // Device cameras (#323): host cameras, virtual cameras and synthetic sources (`sim-camera`).
    (
        "camera_sources",
        "kind?(host|virtual|synthetic)",
        "The sources a device camera can link to, each with the `source` id camera_link takes: the host's cameras (built-in, \
         external, Continuity Camera, and virtual cameras such as OBS Virtual Camera, flagged `virtual`) and the synthetic kinds \
         (pattern:testcard, qr:, text:, image:, video: on macOS, solid:); and the host's camera permission (macOS: the TCC status of the app that \
         started the server, with the fix when it blocks capture), read without ever prompting",
    ),
    (
        "camera_link",
        "device, facing(back|front|both), source; iOS: set?",
        "Feed a device camera from a source: a source id (webcam:<id>, pattern:testcard, qr:<text>, text:<text>, image:<absolute path>, video:<absolute path> (macOS, looped), \
         solid:<colour>; emulated is the test card, none turns the camera off) or a host camera's name, index or part of its name \
         (obs finds OBS Virtual Camera; an ambiguous one fails listing the matches), saved by its stable id. Saved with the device: \
         it comes back on every boot and survives a server restart. Android: a running device is relinked live through emu's console \
         (`camera link`), a shut-down one gets it at boot (-camera-back, -camera-front); a camera that was off at boot appears only \
         after a reboot (the guest lists its cameras once). iOS: injects the camera shim device-wide and serves the source from the running server (needs `simee serve`; one feed per device, so back and front share it; apps already running need a relaunch)",
    ),
    (
        "camera_unlink",
        "device, facing?(back|front|both); iOS: set?",
        "Remove a device camera's link (default both): it goes back to the AVD's own camera (config.ini hw.camera.*), live on a running \
         device and at every boot",
    ),
    (
        "camera_links",
        "device?; iOS: set?",
        "Each linked device camera (one device, or every device): facing, source (and a host camera's name), live (what the running \
         emulator shows) and state: linked, saved (applies at the next boot), relinked (changed from the console; the next boot \
         restores the saved link), unsaved (linked from the console only), off, source missing (the host no longer lists the camera: \
         the device shows the test card until it is back) or permission missing, with the fix",
    ),
    (
        "network_env",
        "device, set?",
        "Environment that makes one app launch shaped; for simctl, prefix each key with SIMCTL_CHILD_ (not on Android: an app starts \
         from an intent with zygote's environment, and the emulator's NAT shapes the whole device, not one app)",
    ),
    (
        "console",
        "device, command | commands",
        "Emulator console commands (`adb emu`'s protocol: sensor, geo, sms, gsm, finger, power, ...); reply lines",
    ),
    (
        "rotate",
        "device, orientation?(portrait|landscape-right|portrait-upside-down|landscape-left; iOS); iOS: set?",
        "Rotate the device 90° clockwise, or to an orientation (iOS)",
    ),
    (
        "fold",
        "device, folded(true|false) | angle(0-180; 0-360 dual-screen) | posture(closed|half-opened|opened|flipped|tent); iOS: set?",
        "Set a foldable's hinge. Android: as the console's fold, unfold and posture do (the hinge sensor, the posture and, folded, \
         the outer screen). iOS: an iPhone Duo's (closed, half-opened is the 100° book posture, opened), which SpringBoard sweeps \
         to, moving the content to the inner screen when it opens past its swap boundary. Returns angle, posture, folded and the \
         size the guest lays out for (iOS: of the screen in use, named in display)",
    ),
    (
        "xr",
        "device, action(recenter | rotate|move: x, y, z | options: environment(day|night), passthrough(0-1) | input: mode(mouse|hand|eye) | viewport: mode(pan|zoom|rotate) | pointer: x, y, phase?(move|down|up|click) | hand|eye: x, y, press?(aim|press) | gesture: gesture(left-pinch|left-grab|left-poke|right-pinch|right-grab|right-poke))",
        "Android XR: turn (radians) or move (metres) the head, recenter, set the room and passthrough, the input mode, aim or pinch the hand ray and look or select with gaze at pixels (stock's gRPC hand and eye events), a hand gesture, or point the dual-mode mouse",
    ),
    (
        "touchpad",
        "device, x, y, phase?(down|move|up|tap) | x, y, to_x, to_y, duration_ms? (a swipe)",
        "Touch or swipe the touchpad beside the screen, in touchpad units (Android XR display glasses: `hw.touchpad0`)",
    ),
    (
        "stylus",
        "device, x, y, phase?(tap|hover|down|move|up|leave), pressure?(1-255), button?, eraser? | x, y, to_x, to_y, duration_ms? (a stroke); iOS: set?",
        "A stylus on the touch screen at pixels. Android: what the stock emulator's gRPC pen events send: hover over it, \
         touch, move, lift or leave, with the barrel button or the eraser end; or draw a stroke. iPad: the Apple Pencil \
         (UITouch type .pencil): tap, down, move, up or a stroke; hover, pressure, button and eraser are refused (the \
         simulator's touch screen has none)",
    ),
    (
        "rotary",
        "device, ticks | delta (Digital Crown units; iOS); iOS: set?",
        "Turn a watch's rotary input: the Wear OS crown or bezel, the Apple Watch Digital Crown",
    ),
    (
        "vehicle",
        "device, property?(VHAL name or id: GEAR_SELECTION, gear, speed, 0x11600207, ...), area_id?, value?",
        "Android Automotive's vehicle HAL: list every property's value, read one, or set one when `value` is given \
         (typed by the property: drive, 12.5, true); through the emulator console's `car` command (simee)",
    ),
    ("watch_gesture", "device, gesture(double-tap|wrist-flick), set?", "An Apple Watch hand gesture"),
    ("remote_swipe", "device, direction(up|down|left|right), duration_ms?, set?", "Swipe on the Siri Remote's touch surface (Apple TV)"),
    (
        "pointer",
        "device, x, y, action?(move|click|down|up), button?(primary|secondary), hold_ms? | x, y, to_x, to_y, button? (a drag), set?",
        "The iPad's pointer (a mouse, as Simulator.app's pointer capture connects): move it to pixels and hover there, click, \
         press or release a button there, or drag to to_x, to_y. Exact on iOS 26.5, within ~20 pt on 27.2. A button held by \
         `down` keeps the pointer's place for the next calls on a server (`simee serve`) until a reboot; `down` without a \
         server, `up` with nothing held, and `move`/`up` after a failed call lost the held button are refused",
    ),
    (
        "carplay",
        "device, on, set?",
        "Attach (or detach) the CarPlay display to a booted iPhone (iOS 26 runtimes); then tap/swipe/screenshot with display=carplay",
    ),
];

/// Per-connection state: each backend's service connection, created on first use.
#[derive(Default)]
pub struct Session {
    pub(crate) ios: crate::ios::Session,
    pub(crate) android: crate::android::Session,
    pub(crate) prefs: crate::prefs::Session,
    pub(crate) camera: crate::camera::Session,
    /// A host state admission judges by instead of this host's (and the seed costs).
    #[cfg(test)]
    pub(crate) fake_host: Option<sim_host::admission::HostState>,
    /// The iOS device set the environment names, instead of `SIMEE_IOS_DEVICE_SET`'s.
    #[cfg(test)]
    pub(crate) fake_env_set: Option<Option<String>>,
}

impl Session {
    /// A session whose Android backend uses this SDK and AVD home instead of the environment's.
    pub fn with_android(android: sim_android::Android) -> Session {
        Session { android: crate::android::Session::with(android), ..Session::default() }
    }

    /// This session with the host's cameras replaced by these, (id, name, virtual) each, and the
    /// camera permission authorized or denied: for front ends' tests of the camera methods.
    #[doc(hidden)]
    pub fn with_cameras(self, cameras: &[(&str, &str, bool)], authorized: bool) -> Session {
        Session { camera: crate::camera::Session::fixed(cameras, authorized), ..self }
    }
}

/// Methods that work on every platform's devices (or need no device).
const ALL_PLATFORMS: &[&str] = &[
    "help",
    "identity",
    "host",
    "devices_usage",
    "form_factors",
    "devices",
    "runtimes",
    "create",
    "delete",
    "boot",
    "boot_many",
    "admission_check",
    "clone",
    "shutdown",
    "screen",
    "screenshot",
    "stream_start",
    "stream_status",
    "stream_stop",
    "streams",
    "contrast_check",
    "tap",
    "swipe",
    "touch",
    "button",
    "type",
    "key",
    "open_url",
    "install",
    "uninstall",
    "launch",
    "terminate",
    "apps",
    // Over adb on Android (#414).
    "processes",
    "kill",
    "exec",
    // Over the emulator's console and adb on Android (#416).
    "connections",
    "traffic",
    "network_shape",
    "network_state",
    "network_clear",
    // Android: the guest's /proc and package manager over adb; emu's hardware definitions (#420).
    "settle",
    "profile",
    "device_types",
    "location",
    "info",
    "rotate",
    "fold",
    "rotary",
    "stylus",
    "prefs_stores",
    "prefs_read",
    "prefs_write",
    "prefs_delete",
    "prefs_batch",
    "prefs_watch",
    // App files: Android through run-as or root (`android_files.rs`).
    "containers",
    "ls",
    "read",
    "write",
    "rm",
    "du",
    "clear_container",
    // Needs no device: the host's camera sources.
    "camera_sources",
    // Camera links: Android through emu, iOS through the device-wide camera shim.
    "camera_link",
    "camera_unlink",
    "camera_links",
    // Android: the guest's media stream, over adb (#417).
    "audio_state",
    "audio_volume",
    "audio_mute",
];
/// Methods only Android devices have.
const ANDROID_ONLY: &[&str] = &["console", "xr", "vehicle", "touchpad", "export", "import"];

/// The platforms whose devices `method` works on.
pub fn platforms(method: &str) -> &'static [Platform] {
    if ALL_PLATFORMS.contains(&method) {
        &Platform::ALL
    } else if ANDROID_ONLY.contains(&method) {
        &[Platform::Android]
    } else {
        &[Platform::Ios]
    }
}

/// Resolves the `device` parameter to a platform and an exact id. With no Android devices around
/// (and no `platform` asking for one), the query goes to the iOS backend unchanged, which resolves
/// it exactly as it always has.
pub(crate) fn resolve(s: &mut Session, p: &Value) -> Result<(Platform, String), String> {
    let query = str_param(p, "device").unwrap_or("booted");
    let only = opt_platform(p)?;
    // An AVD's name is its UDID: resolve it without listing every device on both platforms
    // (AVD names have no spaces, simulator names usually do; `platform` decides otherwise).
    if only != Some(Platform::Ios) && s.android.backend().has_avd(query) {
        return Ok((Platform::Android, query.to_owned()));
    }
    // A simulator's UDID that isn't an AVD's name: the iOS backend looks it up itself. Listing
    // both platforms here (every AVD's config, every simulator's state) cost ~70 ms per call on
    // a host with 48 AVDs and 100 simulators, more than the tap or screenshot it routed.
    if only != Some(Platform::Android) && sim_core::device::is_udid(query) {
        return Ok((Platform::Ios, query.to_owned()));
    }
    // `booted` loads only the running AVDs' configs, not all of them.
    let (android, any) = match only {
        Some(Platform::Ios) => (Vec::new(), false),
        _ => {
            let (devices, any) = s.android.backend().candidates(query);
            (devices.into_iter().map(|d| d.info).collect(), any)
        }
    };
    if !any && only != Some(Platform::Android) {
        return Ok((Platform::Ios, query.to_owned()));
    }
    // Only what `select` reads of each simulator (UDID, name, state); the iOS backend reads the
    // chosen one's full info itself.
    let mut all = if only == Some(Platform::Android) { Vec::new() } else { crate::ios::selection_infos(&mut s.ios)? };
    all.extend(android);
    match select(&all, query) {
        Selection::One(i) => Ok((all[i].platform, all[i].udid.clone())),
        Selection::None => Err(format!("no device matches `{query}`")),
        Selection::Ambiguous => Err(format!("`{query}` matches several devices; use a UDID or `platform`")),
    }
}

/// Fails with the "no device matches" error when `query` was routed to iOS only by its UDID shape
/// and no such simulator exists. [`resolve`] doesn't look the simulator up (that listing costs more
/// than the call it routes), so a platform error that would be about a mistyped UDID checks first.
pub(crate) fn ios_device_exists(s: &mut Session, platform: Platform, query: &str) -> Result<(), String> {
    if platform != Platform::Ios || !sim_core::device::is_udid(query) {
        return Ok(());
    }
    let found = crate::ios::selection_infos(&mut s.ios)?.iter().any(|d| d.udid.eq_ignore_ascii_case(query));
    if found { Ok(()) } else { Err(format!("no device matches `{query}`")) }
}

/// A listing from the iOS backend (empty where there is none).
fn ios_list<T: serde::de::DeserializeOwned>(s: &mut Session, method: &str) -> Result<Vec<T>, String> {
    serde_json::from_value(crate::ios::call(&mut s.ios, method, &json!({}))?).map_err(|e| e.to_string())
}

/// Dispatches one method. `boot`, `boot_many`, `create` and `clone` go through admission first.
/// For a caller that watches (MCP), the long methods report their steps and stop at their next
/// safe point when cancelled (#297).
pub fn call(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    let p = if p.is_null() { &Value::Object(Default::default()) } else { p };
    let set = device_set(s, p);
    s.ios.use_set(set);
    match method {
        "boot" | "boot_many" | "create" | "clone" => admitted(s, method, p),
        "admission_check" => {
            let (platform, form_factor, count) = check_params(p)?;
            let items = vec![Item { platform, form_factor: Some(form_factor) }; count];
            // Nothing says how many boot at once: all of them, the worst case.
            serde_json::to_value(judge_now(s, &items, count)).map_err(|e| e.to_string())
        }
        m if STEPPED.contains(&m) && has_call_hooks() => stepped(s, m, p),
        _ => dispatch(s, method, p),
    }
}

/// The iOS device set the call uses: its `set`, else the environment's, else the default set
/// (#409). An unusable one fails only the methods that connect to the simulator service.
fn device_set(s: &Session, p: &Value) -> Result<Option<String>, String> {
    #[cfg(test)]
    if let Some(env) = &s.fake_env_set {
        return crate::params::device_set_with(p, env.as_deref());
    }
    let _ = s;
    crate::params::device_set(p)
}

/// The iOS device set the environment names (`SIMEE_IOS_DEVICE_SET`, else the default set): the
/// one a call naming no set uses, and the one a camera's `device:` source is found in.
pub(crate) fn environment_set(s: &Session) -> Result<Option<String>, String> {
    device_set(s, &Value::Null)
}

/// How a lifecycle call heeds the admission verdict (#275).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Report it with the result (the default: never blocks).
    Ignore,
    /// Fail fast, doing nothing, unless the verdict is admit.
    Refuse,
    /// Wait until the host admits the devices (`boot`, `boot_many`).
    Wait,
}

fn admission_mode(method: &str, p: &Value) -> Result<Mode, String> {
    match p.get("admission") {
        None | Some(Value::Null) => Ok(Mode::Ignore),
        Some(Value::String(m)) => match m.as_str() {
            "ignore" => Ok(Mode::Ignore),
            "refuse" => Ok(Mode::Refuse),
            "wait" if matches!(method, "boot" | "boot_many") => Ok(Mode::Wait),
            "wait" => Err(format!("`{method}` takes admission ignore or refuse; wait paces boots (`boot`, `boot_many`)")),
            other => Err(format!("unknown admission `{other}` (ignore, refuse or wait)")),
        },
        Some(other) => Err(format!("admission must be ignore, refuse or wait, not {other}")),
    }
}

/// `admission_check`'s platform, form factor (default phone) and count (default 1).
pub(crate) fn check_params(p: &Value) -> Result<(Platform, FormFactor, usize), String> {
    let platform = opt_platform(p)?.ok_or("give `platform`: ios or android")?;
    let form_factor = opt_form_factor(p)?.unwrap_or(FormFactor::Phone);
    Kind::new(platform, form_factor).map_err(|e| e.to_string())?;
    let count = match p.get("count") {
        None | Some(Value::Null) => 1,
        Some(v) => v.as_u64().filter(|n| (1..=1000).contains(n)).ok_or("count must be a whole number from 1 to 1000")? as usize,
    };
    Ok((platform, form_factor, count))
}

/// How long `admission: "wait"` waits (`admission_timeout_s`, default 600 s).
fn wait_options(p: &Value) -> Result<Wait, String> {
    let timeout = match p.get("admission_timeout_s") {
        None | Some(Value::Null) => Wait::DEFAULT_TIMEOUT,
        Some(v) => Duration::from_secs(v.as_u64().ok_or("admission_timeout_s must be a whole number of seconds")?),
    };
    Ok(Wait { timeout, poll: Wait::POLL })
}

/// The refusal `admission: "refuse"` fails with: the verdict and every reason.
fn gate(mode: Mode, a: &Admission) -> Result<(), String> {
    if mode == Mode::Refuse && a.verdict != Verdict::Admit {
        return Err(format!(
            "admission {}: {} (nothing was done; admission \"ignore\" goes ahead anyway)",
            verdict_name(a.verdict),
            a.reasons.join("; ")
        ));
    }
    Ok(())
}

/// The admission verdict on starting `items` on this host now. Each call first lets the
/// estimate learn from the idle devices running.
/// At most `concurrency` of them boot at once.
fn judge_now(s: &mut Session, items: &[Item], concurrency: usize) -> Admission {
    #[cfg(test)]
    if let Some(h) = &s.fake_host {
        return admit(h, items, concurrency, sim_host::admission::seed, &Limits::default());
    }
    observe_running(s);
    let host = HostState::from_report(&sim_host::global().report(sim_host::Thresholds::default()));
    let est = sim_host::admission::estimator().lock().unwrap_or_else(|e| e.into_inner()).clone();
    admit(&host, items, concurrency, |p, f| est.cost(p, f), &Limits::default())
}

/// Feeds the running devices' CPU and footprint (labelled with their platform and form factor
/// as `devices_usage` does) to the process's estimate, without waiting for a rate.
fn observe_running(s: &mut Session) {
    let report = sim_host::global().usage_now();
    if !report.devices.iter().any(|d| d.cpu_pct.m1.is_some()) {
        return;
    }
    let ios: Vec<DeviceInfo> = if report.devices.iter().any(|d| d.kind == Some(sim_host::usage::Kind::Simulator)) {
        crate::ios::call(&mut s.ios, "devices", &json!({"booted": true}))
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let android = s.android.backend();
    let Ok(rows) = crate::usage::rows(&report, &ios, |avd| android.running_info(avd)) else { return };
    let mut est = sim_host::admission::estimator().lock().unwrap_or_else(|e| e.into_inner());
    for row in rows["devices"].as_array().into_iter().flatten() {
        let platform = row["platform"].as_str().and_then(Platform::parse);
        let form_factor = row["form_factor"].as_str().and_then(FormFactor::parse);
        if let (Some(platform), Some(form_factor)) = (platform, form_factor) {
            est.observe(platform, form_factor, row["cpu_pct"]["1m"].as_f64(), row["footprint_bytes"].as_u64().unwrap_or(0));
        }
    }
}

/// A device a lifecycle call names: its exact id, platform, form factor and whether it runs.
struct Target {
    udid: String,
    item: Item,
    running: bool,
}

fn target(s: &mut Session, query: &str, platform: Option<&Value>) -> Result<Target, String> {
    let info = dispatch(s, "info", &json!({"device": query, "platform": platform}))?;
    let d: DeviceInfo = serde_json::from_value(info["device"].clone()).map_err(|e| e.to_string())?;
    let running = matches!(d.state, DeviceState::Booted | DeviceState::Booting);
    Ok(Target { udid: d.udid, item: Item { platform: d.platform, form_factor: d.form_factor }, running })
}

/// `result` with `admission` (and `pacing`) in it; a `null` result becomes an object of them.
fn with_admission(result: Value, a: &Admission, pacing: Option<Value>) -> Value {
    let mut r = match result {
        Value::Null => json!({}),
        Value::Object(_) => result,
        other => return other,
    };
    r["admission"] = serde_json::to_value(a).unwrap_or_default();
    if let Some(p) = pacing {
        r["pacing"] = p;
    }
    r
}

fn pacing_json<R>(p: &Paced<R>) -> Value {
    json!({"waited_s": p.waited_s, "timed_out": p.timed_out, "cancelled": p.cancelled, "steps": p.steps})
}

/// What a waiting call says while it waits: a line on stderr (stdout is `simee mcp`'s protocol)
/// and the caller's progress hook.
fn progress(method: &str, step: &Step) {
    if step.started == 0 {
        let line =
            format!("{method}: {} waiting at {} s, admission {}: {}", step.waiting, step.at_s, verdict_name(step.verdict), step.reason);
        eprintln!("simee: {line}");
        call_progress(&line);
    }
}

/// `boot`, `create` and `clone` with their admission: judged before the call (a device already
/// running costs nothing), refused or waited on as `admission` says, and returned with the result.
/// A device that doesn't resolve goes straight to the method, which says why. A watched call
/// reports its start and end as steps; a cancelled `boot` shuts down the device it was booting,
/// and a `create` or `clone` cancelled while it ran deletes the device it made.
fn admitted(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    let mode = admission_mode(method, p)?;
    if method == "boot_many" {
        return boot_many_admitted(s, p, mode);
    }
    // The device this `boot` starts (it isn't running), with its platform.
    let mut booting = None;
    let items = match method {
        "create" => {
            let platform = opt_platform(p)?.unwrap_or(Platform::Ios);
            let form_factor = opt_form_factor(p)?;
            if let Some(f) = form_factor {
                Kind::new(platform, f).map_err(|e| e.to_string())?;
            }
            vec![Item { platform, form_factor }]
        }
        _ => match target(s, str_param(p, "device").unwrap_or("booted"), p.get("platform")) {
            Ok(t) if method == "boot" && t.running => Vec::new(),
            Ok(t) => {
                if method == "boot" {
                    booting = Some((t.udid.clone(), t.item.platform));
                }
                vec![t.item]
            }
            Err(_) => return dispatch(s, method, p),
        },
    };
    let (doing, did) = phase_words(method);
    let subject = subject(method, p);
    call_step(doing, 0, 1, format!("{doing} {subject}"));
    let mut a = judge_now(s, &items, 1);
    gate(mode, &a)?;
    let mut pacing = None;
    if mode == Mode::Wait && !items.is_empty() {
        let wait = wait_options(p)?;
        let paced = pace(
            &items,
            |_| true,
            &SystemClock::start(),
            wait,
            call_cancelled,
            |rest| judge_now(s, rest, 1),
            |_| (),
            |step| progress(method, step),
        );
        if paced.timed_out || paced.cancelled {
            let last = paced.last.as_ref().unwrap_or(&a);
            let what = if paced.cancelled { "cancelled after waiting" } else { "the host didn't admit the boot within" };
            return Err(format!(
                "admission: {what} {} s ({}: {}); nothing was booted",
                paced.waited_s,
                verdict_name(last.verdict),
                last.reasons.join("; ")
            ));
        }
        pacing = Some(pacing_json(&paced));
        a = paced.last.unwrap_or(a);
    }
    if call_cancelled() {
        return Err(format!("`{method}` was cancelled before {doing} {subject}; nothing was done"));
    }
    let result = match booting {
        Some((udid, platform)) if has_call_hooks() => boot_watched(s, p, udid, platform)?,
        _ => dispatch(s, method, p)?,
    };
    if matches!(method, "create" | "clone") && call_cancelled() {
        return Err(undo_made(s, method, &result));
    }
    call_step(did, 1, 1, format!("{did} {subject}"));
    // A new device's form factor, where the call didn't name it, is known now.
    if method == "create" {
        let made = result["form_factor"].as_str().and_then(FormFactor::parse);
        if made.is_some() && items.first().is_some_and(|i| i.form_factor != made) {
            a = judge_now(s, &[Item { platform: items[0].platform, form_factor: made }], 1);
        }
    }
    Ok(with_admission(result, &a, pacing))
}

/// `boot_many` with its admission. `wait` boots the devices in chunks as the host admits them,
/// each chunk as `boot_many` boots its devices, and reports the ones it never started. Every call
/// boots through its backend's window ([`boot_window`]); a watched one hears each device done.
fn boot_many_admitted(s: &mut Session, p: &Value, mode: Mode) -> Result<Value, String> {
    let queries = crate::params::strings(p, "devices");
    let targets: Result<Vec<Target>, String> = queries.iter().map(|q| target(s, q, p.get("platform"))).collect();
    // Unknown devices, none, or several platforms: the method says why.
    let targets = match targets {
        Ok(t) if !t.is_empty() && t.iter().all(|x| x.item.platform == t[0].item.platform) => t,
        _ => return dispatch(s, "boot_many", p),
    };
    if targets[0].item.platform == Platform::Android {
        // Saved `device:` links' feeds are served first, as `boot`'s are.
        let udids: Vec<String> = targets.iter().map(|t| t.udid.clone()).collect();
        crate::camera::before_boot_many(s, &udids);
    }
    let new = |ts: &[Target]| -> Vec<Item> { ts.iter().filter(|t| !t.running).map(|t| t.item).collect() };
    let concurrency = crate::params::concurrency(p)?;
    let tally = std::cell::RefCell::new(Tally { done: 0, failed: 0, total: targets.len() as u64 });
    tally.borrow().step("booting", format!("0 of {} devices booted", targets.len()));
    let a = judge_now(s, &new(&targets), concurrency);
    gate(mode, &a)?;
    let fleet = Fleet::of(s);
    if mode != Mode::Wait {
        let result = boot_window(&fleet, p, &targets, concurrency, &mut tally.borrow_mut())?;
        return Ok(with_admission(result, &a, None));
    }
    let wait = wait_options(p)?;
    let clock = SystemClock::start();
    let session = std::cell::RefCell::new(s);
    let chunks = std::cell::RefCell::new(Vec::<Vec<String>>::new());
    let paced = pace(
        &targets,
        |t| !t.running,
        &clock,
        wait,
        call_cancelled,
        |rest| judge_now(&mut session.borrow_mut(), &new(rest), concurrency),
        |chunk| {
            chunks.borrow_mut().push(chunk.iter().map(|t| t.udid.clone()).collect());
            boot_window(&fleet, p, chunk, concurrency, &mut tally.borrow_mut())
        },
        |step| progress("boot_many", step),
    );
    let (mut devices, mut failed, unsettled) = merge_chunks(&paced.results, &chunks.borrow())?;
    let last = paced.last.as_ref().unwrap_or(&a);
    for t in &targets[targets.len() - paced.not_started..] {
        let why = last.reasons.first().map_or("", String::as_str);
        let error = if paced.cancelled {
            format!(
                "not started: cancelled while waiting for the host to admit it, after {} s ({}: {why})",
                paced.waited_s,
                verdict_name(last.verdict)
            )
        } else {
            format!("not started: the host didn't admit it within {} s of waiting ({}: {why})", paced.waited_s, verdict_name(last.verdict))
        };
        devices.push(json!({"device": t.udid, "error": error}));
        failed += 1;
    }
    let all_s = (clock.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    let mut result = json!({"concurrency": concurrency, "all_s": all_s, "failed": failed, "unsettled": unsettled, "devices": devices});
    if paced.cancelled || paced.results.iter().any(|r| r.as_ref().is_ok_and(|r| r["cancelled"] == true)) {
        result["cancelled"] = true.into();
    }
    Ok(with_admission(result, &a, Some(pacing_json(&paced))))
}

/// The devices, failed and unsettled counts of the chunks `boot_many` booted one after another
/// (`ids` are each chunk's devices). A chunk that fails as a whole (no ports left, a plan error)
/// fails its own devices and the chunks before it stay in the result, running; only when every
/// chunk failed is it the call's error.
fn merge_chunks(results: &[Result<Value, String>], ids: &[Vec<String>]) -> Result<(Vec<Value>, u64, u64), String> {
    if let Some(Err(e)) = results.first().filter(|_| results.iter().all(Result::is_err)) {
        return Err(e.clone());
    }
    let (mut devices, mut failed, mut unsettled) = (Vec::new(), 0, 0);
    for (r, ids) in results.iter().zip(ids) {
        match r {
            Ok(r) => {
                devices.extend(r["devices"].as_array().cloned().unwrap_or_default());
                failed += r["failed"].as_u64().unwrap_or(0);
                unsettled += r["unsettled"].as_u64().unwrap_or(0);
            }
            Err(e) => {
                devices.extend(ids.iter().map(|id| json!({"device": id, "error": format!("boot_many failed for its chunk: {e}")})));
                failed += ids.len() as u64;
            }
        }
    }
    Ok((devices, failed, unsettled))
}

// Progress and cancellation for a watched call (#297): the devices a `boot_many` has done, the
// phases of the other long methods, and what a cancel stops and undoes.

/// What a watched lifecycle call does its work with off the caller's thread: sessions of its own
/// (a session's iOS service connection stays on the thread that made it), over the caller's
/// Android SDK and AVD home. A fake in tests.
trait Booter: Send + Sync + 'static {
    /// Runs `method` (`boot`) on a session of this thread's own.
    fn run(&self, method: &str, p: &Value) -> Result<Value, String>;
    /// `boot_many` of `p`'s devices on a session of this thread's own, its backend's window asking
    /// and telling `hook` about each device.
    fn boot_many(&self, p: &Value, hook: &dyn BootHook) -> Result<Value, String>;
    /// Shuts down these devices of `platform`, at once (each on its own thread); errors are
    /// ignored (one that never came up has nothing to stop).
    fn shut_down(&self, platform: Platform, ids: &[String]);
}

#[derive(Clone)]
struct Fleet {
    android: sim_android::Android,
}

impl Fleet {
    fn of(s: &mut Session) -> std::sync::Arc<Fleet> {
        std::sync::Arc::new(Fleet { android: s.android.backend().clone() })
    }

    fn session(&self) -> Session {
        Session::with_android(self.android.clone())
    }
}

impl Booter for Fleet {
    fn run(&self, method: &str, p: &Value) -> Result<Value, String> {
        dispatch(&mut self.session(), method, p)
    }

    fn boot_many(&self, p: &Value, hook: &dyn BootHook) -> Result<Value, String> {
        boot_many_hooked(&mut self.session(), p, hook)
    }

    fn shut_down(&self, platform: Platform, ids: &[String]) {
        std::thread::scope(|scope| {
            for id in ids {
                scope.spawn(move || {
                    let _ = dispatch(&mut self.session(), "shutdown", &json!({"device": id, "platform": platform.as_str()}));
                });
            }
        });
    }
}

/// How far a watched `boot_many` is: devices done (booted, or failed) and failed, of all.
struct Tally {
    done: u64,
    failed: u64,
    total: u64,
}

impl Tally {
    fn step(&self, phase: &str, message: String) {
        call_step(phase, self.done, self.total, message);
    }

    fn failed(&self) -> String {
        if self.failed > 0 { format!(" ({} failed)", self.failed) } else { String::new() }
    }
}

/// What a `boot_many` knows of its backend's window while it runs: which devices have started,
/// which this call booted, which finished (with their entries), and, once its caller cancelled,
/// why no more start. The window's threads reach it as their [`BootHook`].
struct Window<B: Booter> {
    fleet: std::sync::Arc<B>,
    platform: Platform,
    ids: Vec<String>,
    state: std::sync::Mutex<WindowState>,
}

struct WindowState {
    started: Vec<bool>,
    /// Which devices this call booted: the backend found each off as its turn came and launched
    /// it ([`BootHook::launching`]). Only these are its to shut down; one another caller booted
    /// meanwhile, or one that ran already, never is.
    ours: Vec<bool>,
    done: Vec<Option<Value>>,
    stop: Option<String>,
}

impl WindowState {
    /// Devices finished (booted or failed) and failed.
    fn counts(&self) -> (u64, u64) {
        let done = self.done.iter().flatten();
        (done.clone().count() as u64, done.filter(|d| d.get("error").is_some()).count() as u64)
    }
}

impl<B: Booter> Window<B> {
    fn state(&self) -> std::sync::MutexGuard<'_, WindowState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn counts(&self) -> (u64, u64) {
        self.state().counts()
    }
}

impl<B: Booter> BootHook for Window<B> {
    fn start(&self, i: usize) -> Result<(), String> {
        let mut st = self.state();
        if let Some(why) = &st.stop {
            return Err(why.clone());
        }
        if let Some(s) = st.started.get_mut(i) {
            *s = true;
        }
        Ok(())
    }

    fn launching(&self, i: usize) -> Result<(), String> {
        // Under the lock the cancel takes its snapshot with: a device is this call's before the
        // cancel looks (and is shut down by it), or it is never launched.
        let mut st = self.state();
        if let Some(why) = &st.stop {
            return Err(why.clone());
        }
        if let Some(o) = st.ours.get_mut(i) {
            *o = true;
        }
        Ok(())
    }

    fn finished(&self, i: usize, device: &Value) {
        let late = {
            let mut st = self.state();
            if let Some(d) = st.done.get_mut(i) {
                *d = Some(device.clone());
            }
            st.stop.is_some() && st.ours.get(i) == Some(&true) && device.get("error").is_none()
        };
        // Booted after its caller cancelled (the shutdown then may have come too early to stop
        // it): this call started it, so it is shut down.
        if late && let Some(id) = self.ids.get(i) {
            self.fleet.shut_down(self.platform, std::slice::from_ref(id));
        }
    }
}

/// `boot_many` of `targets` (one platform; `p`'s options): one backend `boot_many` of them all,
/// whose window keeps `concurrency` devices booting and starts the next as soon as any finishes,
/// with this call's [`Window`] as its hook. Whether or not a caller watches it runs the same; a
/// watched one runs on a thread of its own ([`interruptible_watching`]), so the caller hears each
/// device done and a cancel is seen at once. Cancelled, no more devices start, the ones this call
/// booted that are still booting are shut down (one running or booting already when its turn
/// came, another caller's, is left as it was), and the result has the devices done, every other one's reason, and `cancelled: true`. The
/// backend failing as a whole (a parameter it refuses) is the call's error.
fn boot_window(
    fleet: &std::sync::Arc<impl Booter>,
    p: &Value,
    targets: &[Target],
    concurrency: usize,
    tally: &mut Tally,
) -> Result<Value, String> {
    let platform = targets.first().map_or(Platform::Ios, |t| t.item.platform);
    let ids: Vec<String> = targets.iter().map(|t| t.udid.clone()).collect();
    let n = ids.len();
    let window = std::sync::Arc::new(Window {
        fleet: fleet.clone(),
        platform,
        ids: ids.clone(),
        state: std::sync::Mutex::new(WindowState { started: vec![false; n], ours: vec![false; n], done: vec![None; n], stop: None }),
    });
    let mut q = p.clone();
    q["devices"] = ids.into_iter().map(Value::String).collect();
    q["platform"] = json!(platform.as_str());
    q["concurrency"] = json!(concurrency);
    let start = std::time::Instant::now();
    let base = (tally.done, tally.failed);
    let heard = |tally: &mut Tally, window: &Window<_>| {
        let (done, failed) = window.counts();
        if base.0 + done == tally.done {
            return false;
        }
        (tally.done, tally.failed) = (base.0 + done, base.1 + failed);
        true
    };
    let theirs = window.clone();
    let got = interruptible_watching(
        "boot_many",
        move |h: &Handoff<Result<Value, String>>| {
            // Devices that finish after the caller left are shut down by the hook as each does.
            h.give(theirs.fleet.boot_many(&q, &*theirs));
        },
        || {
            if heard(tally, &window) {
                tally.step("booted", format!("{} of {} done{}", tally.done, tally.total, tally.failed()));
            }
        },
    )?;
    if let Some(r) = got {
        let r = r?;
        if heard(tally, &window) {
            tally.step("booted", format!("{} of {} done{}", tally.done, tally.total, tally.failed()));
        }
        return Ok(r);
    }
    // One snapshot decides what is shut down, what `devices` says and what the last step counts.
    let (devices, booting, (done, failed)) = {
        let mut st = window.state();
        st.stop = Some("not started: boot_many was cancelled".into());
        let mut booting = Vec::new();
        let devices: Vec<Value> = targets
            .iter()
            .enumerate()
            .map(|(i, t)| match (&st.done[i], st.started[i], st.ours[i]) {
                (Some(d), ..) => d.clone(),
                (None, _, true) => {
                    booting.push(t.udid.clone());
                    json!({"device": t.udid, "error": "cancelled while booting: shut down (this call had started it)"})
                }
                (None, true, false) => json!({"device": t.udid,
                    "error": "cancelled while boot_many waited on it: this call hadn't booted it, so it was left as it was"}),
                (None, false, false) => json!({"device": t.udid, "error": "not started: boot_many was cancelled"}),
            })
            .collect();
        (devices, booting, st.counts())
    };
    fleet.shut_down(platform, &booting);
    (tally.done, tally.failed) = (base.0 + done, base.1 + failed);
    tally.step("cancelled", format!("cancelled with {} of {} done; shut down {} it was booting", tally.done, tally.total, booting.len()));
    let failed = devices.iter().filter(|d| d.get("error").is_some()).count();
    let unsettled = devices.iter().filter(|d| d["settled"] == json!(false)).count();
    let all_s = (start.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    Ok(json!({"concurrency": concurrency, "all_s": all_s, "failed": failed, "unsettled": unsettled, "devices": devices,
        "cancelled": true}))
}

/// A watched `boot` of `udid`, which wasn't running when it was admitted: the boot runs on a
/// thread of its own, and a cancel fails the call and shuts the device down at once if this call
/// booted it (the backend found it off and launched it), never if another caller had since.
fn boot_watched(s: &mut Session, p: &Value, udid: String, platform: Platform) -> Result<Value, String> {
    // Camera links are saved first, on this session, as `dispatch` would; the boot reads them.
    crate::camera::before_boot(s, p)?;
    let fleet = Fleet::of(s);
    let mut q = p.clone();
    if let Some(fields) = q.as_object_mut() {
        fields.remove("camera_back");
        fields.remove("camera_front");
    }
    q["device"] = json!(udid);
    q["platform"] = json!(platform.as_str());
    boot_one(&fleet, q, udid, platform)
}

/// [`boot_watched`]'s boot of `udid` by `q`, with `fleet`.
fn boot_one(fleet: &std::sync::Arc<impl Booter>, q: Value, udid: String, platform: Platform) -> Result<Value, String> {
    // Cancelled, and whether this call launched the device: the backend asks to launch under the
    // lock the cancel looks with, so the device is this call's before the cancel looks or is never
    // launched.
    #[derive(Default)]
    struct Launch {
        cancelled: bool,
        launched: bool,
    }
    fn lock(m: &std::sync::Mutex<Launch>) -> std::sync::MutexGuard<'_, Launch> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }
    let launch = std::sync::Arc::new(std::sync::Mutex::new(Launch::default()));
    let (theirs, undo, ask) = (fleet.clone(), vec![udid.clone()], launch.clone());
    let got = interruptible("boot", move |h: &Handoff<Result<Value, String>>| {
        let hook = {
            let ask = ask.clone();
            move || {
                let mut st = lock(&ask);
                if st.cancelled {
                    return Err("not booted: boot was cancelled".to_owned());
                }
                st.launched = true;
                Ok(())
            }
        };
        let r = crate::progress::with_launch_hook(hook, || theirs.run("boot", &q));
        // Came up after its caller left: shut down if this call booted it.
        if h.give(r).is_some() && lock(&ask).launched {
            theirs.shut_down(platform, &undo);
        }
    })?;
    if let Some(r) = got {
        return r;
    }
    let launched = {
        let mut st = lock(&launch);
        st.cancelled = true;
        st.launched
    };
    if launched {
        fleet.shut_down(platform, std::slice::from_ref(&udid));
        Err(format!("boot was cancelled while {udid} was booting; this call had started it, so it was shut down"))
    } else {
        Err(format!("boot was cancelled while it waited on {udid}, which this call hadn't booted (another had), so it was left as it was"))
    }
}

/// The error of a `create`, `clone` or `import` cancelled while it ran: the device it made is
/// deleted (the result is it, or `import`'s `{device}`).
fn undo_made(s: &mut Session, method: &str, result: &Value) -> String {
    let made = if method == "import" { &result["device"] } else { result };
    let Some(udid) = made["udid"].as_str() else { return format!("`{method}` was cancelled") };
    match dispatch(s, "delete", &json!({"device": udid, "platform": made["platform"]})) {
        Ok(_) => format!("`{method}` was cancelled: it had made {udid}, which was deleted"),
        Err(e) => format!("`{method}` was cancelled: it had made {udid}, which could not be deleted ({e})"),
    }
}

/// The long methods (past admission) a watched call reports the phases of, and stops before
/// starting when cancelled. Their work can't be interrupted from here once it runs.
const STEPPED: &[&str] = &["install", "erase", "settle", "shutdown", "export", "import", "mem_leaks", "mem_heap"];

/// What a long method is doing, and what it has done, in a step's message.
fn phase_words(method: &str) -> (&'static str, &'static str) {
    match method {
        "boot" => ("booting", "booted"),
        "create" => ("creating", "created"),
        "clone" => ("cloning", "cloned"),
        "install" => ("installing", "installed"),
        "erase" => ("erasing", "erased"),
        "settle" => ("settling", "settled"),
        "shutdown" => ("shutting down", "shut down"),
        "export" => ("exporting", "exported"),
        "import" => ("importing", "imported"),
        "mem_leaks" => ("finding the leaks of", "found the leaks of"),
        "mem_heap" => ("reading the heap of", "read the heap of"),
        _ => ("running", "ran"),
    }
}

/// What a long method works on, for its steps' messages.
fn subject(method: &str, p: &Value) -> String {
    let get = |k: &str| str_param(p, k).unwrap_or("?");
    let device = str_param(p, "device").unwrap_or("booted");
    match method {
        "create" => get("name").to_owned(),
        "clone" => format!("{device} as {}", get("name")),
        "install" => format!("{} on {device}", get("path")),
        "export" => format!("{device} to {}", get("to")),
        "import" => format!("{} as {}", get("from"), get("name")),
        "mem_leaks" | "mem_heap" => match (str_param(p, "bundle_id"), p.get("pid").and_then(Value::as_u64)) {
            (Some(app), _) => format!("{app} on {device}"),
            (None, Some(pid)) => format!("pid {pid} on {device}"),
            _ => device.to_owned(),
        },
        _ => device.to_owned(),
    }
}

/// The bytes under `path` (a file, or a directory's files; links not followed).
fn path_bytes(path: &std::path::Path) -> u64 {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::read_dir(path).into_iter().flatten().flatten().map(|e| path_bytes(&e.path())).sum(),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// A [`STEPPED`] method for a caller that watches: a step as it starts and as it ends, checking
/// for a cancel before the work starts (`install`: after measuring the bundle, its first phase).
/// A cancelled `import` deletes the device it made.
fn stepped(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    let (doing, did) = phase_words(method);
    let mut subject = subject(method, p);
    let phases = if method == "install" { 2 } else { 1 };
    if method == "install" {
        call_step("checking", 0, 2, format!("checking {subject}"));
        if let Some(path) = str_param(p, "path") {
            subject = format!("{subject} ({:.1} MB)", path_bytes(std::path::Path::new(path)) as f64 / 1e6);
        }
    }
    if call_cancelled() {
        return Err(format!("`{method}` was cancelled before {doing} {subject}; nothing was done"));
    }
    call_step(doing, phases - 1, phases, format!("{doing} {subject}"));
    let result = dispatch(s, method, p)?;
    if method == "import" && call_cancelled() {
        return Err(undo_made(s, method, &result));
    }
    call_step(did, phases, phases, format!("{did} {subject}"));
    Ok(result)
}

/// Dispatches one method, past admission.
fn dispatch(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    // `camera_back` / `camera_front` are linked (and saved) first, so the boot passes them to emu.
    if method == "boot" {
        crate::camera::before_boot(s, p)?;
    }
    match method {
        m if crate::camera::is_method(m) => crate::camera::call(s, m, p),
        "help" => Ok(METHODS
            .iter()
            .map(|(m, params, doc)| {
                let platforms: Vec<&str> = platforms(m).iter().map(|p| p.as_str()).collect();
                json!({"method": m, "params": params, "doc": doc, "platforms": platforms})
            })
            .collect()),
        "identity" => Ok(crate::identity().clone()),
        "host" => serde_json::to_value(sim_host::global().report(sim_host::Thresholds::default())).map_err(|e| e.to_string()),
        "devices_usage" => crate::usage::devices_usage(s),
        "form_factors" => {
            let (platform, form_factor) = (opt_platform(p)?, opt_form_factor(p)?);
            Ok(matrix()
                .into_iter()
                .filter(|(f, pl, _)| platform.is_none_or(|x| x == *pl) && form_factor.is_none_or(|x| x == *f))
                .map(|(f, pl, kind)| match kind {
                    Ok(k) => {
                        let inputs: Vec<Value> = k.inputs().iter().map(|i| json!({"input": i, "label": i.label(pl)})).collect();
                        json!({"form_factor": f, "platform": pl, "supported": true, "sub_platform": k.sub_platform, "label": k.label(),
                            "inputs": inputs, "profile": k.profile()})
                    }
                    Err(e) => json!({"form_factor": f, "platform": pl, "supported": false, "reason": e.reason}),
                })
                .collect())
        }
        "devices" => {
            let (platform, form_factor) = (opt_platform(p)?, opt_form_factor(p)?);
            let booted = p.get("booted").and_then(Value::as_bool).unwrap_or(false);
            let mut list: Vec<DeviceInfo> = if platform == Some(Platform::Android) {
                Vec::new()
            } else {
                serde_json::from_value(crate::ios::call(&mut s.ios, method, p)?).map_err(|e| e.to_string())?
            };
            if platform != Some(Platform::Ios) {
                list.extend(s.android.devices().into_iter().filter(|d| !booted || d.state == sim_core::device::DeviceState::Booted));
            }
            let keep = |d: &DeviceInfo| platform.is_none_or(|x| x == d.platform) && form_factor.is_none_or(|x| d.form_factor == Some(x));
            serde_json::to_value(list.into_iter().filter(keep).collect::<Vec<_>>()).map_err(|e| e.to_string())
        }
        "runtimes" => {
            let platform = opt_platform(p)?;
            let mut list: Vec<RuntimeInfo> = if platform == Some(Platform::Android) { Vec::new() } else { ios_list(s, method)? };
            if platform != Some(Platform::Ios) {
                list.extend(s.android.backend().runtimes());
            }
            serde_json::to_value(list).map_err(|e| e.to_string())
        }
        "create" => {
            // Unsupported pairs are rejected before any backend runs.
            let platform = opt_platform(p)?.unwrap_or(Platform::Ios);
            let form_factor = opt_form_factor(p)?;
            if let Some(f) = form_factor {
                Kind::new(platform, f).map_err(|e| e.to_string())?;
            }
            match platform {
                Platform::Ios => crate::ios::call(&mut s.ios, method, p),
                Platform::Android => {
                    let device = crate::params::str_param(p, "device");
                    let info =
                        s.android.backend().create_device(need_str(p, "name")?, need_str(p, "system_image")?, form_factor, device)?;
                    serde_json::to_value(info).map_err(|e| e.to_string())
                }
            }
        }
        // Need no device: resolving the default `booted` would fail with two simulators booted.
        "device_types" => {
            let platform = opt_platform(p)?;
            let mut list: Vec<DeviceTypeInfo> = if platform == Some(Platform::Android) { Vec::new() } else { ios_list(s, method)? };
            if platform != Some(Platform::Ios) {
                list.extend(sim_android::device_types());
            }
            serde_json::to_value(list).map_err(|e| e.to_string())
        }
        "audio_capture_stop" => crate::ios::call(&mut s.ios, method, p),
        // Streams outlive the call: the process's own registry, which resolves `device` itself.
        m if crate::stream::is_method(m) => crate::stream::call(s, m, p),
        // Makes a device from a bundle; the bundle's manifest says whose.
        "import" => match opt_platform(p)? {
            Some(Platform::Ios) => Err("`import` takes Android bundles for now; iOS bundles are the next step of #170".into()),
            _ => crate::android::import(&mut s.android, p),
        },
        "boot_many" => boot_many_hooked(s, p, &crate::progress::Unwatched),
        m if crate::prefs::is_method(m) => {
            let (platform, udid) = resolve(s, p)?;
            ios_device_exists(s, platform, str_param(p, "device").unwrap_or("booted"))?;
            crate::prefs::call(s.prefs.backend_on(platform, &s.ios)?, platform, &udid, m, p)
        }
        m if METHODS.iter().any(|(name, ..)| *name == m) => {
            let (platform, udid) = resolve(s, p)?;
            if !platforms(m).contains(&platform) {
                ios_device_exists(s, platform, str_param(p, "device").unwrap_or("booted"))?;
                let on: Vec<&str> = platforms(m).iter().map(|p| p.label()).collect();
                return Err(format!("`{m}` isn't available on {} devices (it works on {})", platform.label(), on.join(", ")));
            }
            match platform {
                Platform::Ios => {
                    let mut p = p.clone();
                    p["device"] = Value::String(udid);
                    crate::ios::call(&mut s.ios, m, &p)
                }
                Platform::Android => crate::android::call(&mut s.android, m, p, &udid),
            }
        }
        other => Err(format!("unknown method `{other}`; call `help`")),
    }
}

/// `boot_many` past admission: its backend's boot window, with `hook` asked before each device
/// starts and told as each finishes. One platform per call: each backend boots its own devices
/// its own way.
fn boot_many_hooked(s: &mut Session, p: &Value, hook: &dyn crate::progress::BootHook) -> Result<Value, String> {
    let mut udids = Vec::new();
    let mut first: Option<(Platform, String)> = None;
    for q in crate::params::strings(p, "devices") {
        let (platform, udid) = resolve(s, &json!({"device": q, "platform": p.get("platform")}))?;
        match &first {
            None => first = Some((platform, q.clone())),
            Some((other, q0)) if *other != platform => {
                ios_device_exists(s, platform, &q)?;
                ios_device_exists(s, *other, q0)?;
                return Err(format!(
                    "`boot_many` boots one platform's devices per call: `{q0}` is an {} device and `{q}` an {} one",
                    other.label(),
                    platform.label()
                ));
            }
            Some(_) => {}
        }
        udids.push(udid);
    }
    match first.map(|(platform, _)| platform) {
        Some(Platform::Android) => {
            // Saved `device:` links' feeds are served first, as `boot`'s are.
            crate::camera::before_boot_many(s, &udids);
            crate::android::boot_many(&mut s.android, p, &udids, hook)
        }
        _ => {
            let mut p = p.clone();
            p["devices"] = udids.into_iter().map(Value::String).collect();
            crate::ios::boot_many_hooked(&mut s.ios, &p, hook)
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::progress::{CallHooks, with_call_hooks};

    #[test]
    fn host_returns_the_snapshot_averages_and_verdict() {
        let h = call(&mut Session::default(), "host", &Value::Null).expect("host");
        assert!(h["snapshot"]["cpus"].as_u64().is_some_and(|c| c >= 1));
        assert!(h["snapshot"]["load"]["one"].as_f64().is_some_and(|l| l >= 0.0));
        assert!(h["snapshot"]["disk"]["free_bytes"].as_u64().is_some_and(|b| b > 0));
        assert!(h["snapshot"]["memory"]["level"].is_string() && h["snapshot"]["power"]["summary"].is_string());
        assert!(h["snapshot"]["thermal"]["available"].is_boolean());
        assert_eq!(h["averages"].as_array().map(Vec::len), Some(3));
        assert!(matches!(h["verdict"]["verdict"].as_str(), Some("admit" | "hold" | "backoff")));
        assert!(h["reasons"].as_array().is_some_and(|r| !r.is_empty()));
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        assert!(help.as_array().is_some_and(|a| a.iter().any(|m| m["method"] == "host" && m["platforms"] == json!(["android", "ios"]))));
    }

    #[test]
    fn devices_usage_lists_the_running_devices_without_touching_them() {
        let u = call(&mut Session::default(), "devices_usage", &Value::Null).expect("devices_usage");
        assert_eq!(u["available"], cfg!(target_os = "macos"));
        assert!(u["host"]["cpus"].as_u64().is_some_and(|c| c >= 1) && u["simee"]["id"] == "simee");
        let rows = u["devices"].as_array().expect("devices");
        for d in rows {
            assert!(matches!(d["platform"].as_str(), Some("ios" | "android")) && d["name"].is_string(), "{d}");
            assert!(d["pids"].as_u64().is_some_and(|n| n >= 1), "{d}");
            println!("{} {} {}: {} % cpu (1 s), {} bytes", d["platform"], d["kind"], d["name"], d["cpu_pct"]["1s"], d["footprint_bytes"]);
        }
        if cfg!(target_os = "macos") {
            assert!(u["samples"].as_u64().is_some_and(|n| n >= 2), "the call waits for a rate: {u}");
        }
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        assert!(
            help.as_array()
                .is_some_and(|a| a.iter().any(|m| m["method"] == "devices_usage" && m["platforms"] == json!(["android", "ios"])))
        );
    }

    #[test]
    fn help_labels_every_method_with_platforms() {
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        let find = |m: &str| help.as_array().and_then(|a| a.iter().find(|x| x["method"] == m)).cloned().expect(m);
        assert_eq!(find("devices")["platforms"], json!(["android", "ios"]));
        assert_eq!(find("status_bar")["platforms"], json!(["ios"]));
        assert_eq!(find("tap")["platforms"], json!(["android", "ios"]));
        assert_eq!(find("fold")["platforms"], json!(["android", "ios"]));
        assert_eq!(find("xr")["platforms"], json!(["android"]));
        assert_eq!(find("rotary")["platforms"], json!(["android", "ios"]));
        assert_eq!(find("rotate")["platforms"], json!(["android", "ios"]));
        assert_eq!(find("carplay")["platforms"], json!(["ios"]));
        assert_eq!(find("watch_gesture")["platforms"], json!(["ios"]));
        assert_eq!(find("pointer")["platforms"], json!(["ios"]));
        assert_eq!(find("stylus")["platforms"], json!(["android", "ios"]));
        assert!(help.as_array().expect("array").iter().all(|m| m["method"] != "pencil"), "`stylus` is the one pen method");
        assert_eq!(find("vehicle")["platforms"], json!(["android"]));
        assert!(help.as_array().expect("array").iter().all(|m| m["platforms"].as_array().is_some_and(|p| !p.is_empty())));
    }

    /// An SDK with a mobile, a Wear OS and an Android XR image, and an empty AVD home.
    fn android_session(tag: &str) -> (Session, std::path::PathBuf) {
        android_session_in(&std::env::temp_dir(), tag)
    }

    /// [`android_session`] under `/tmp`, for a fake emulator: its display socket's path must fit
    /// a unix socket address (104 bytes on macOS).
    fn short_android_session(tag: &str) -> (Session, std::path::PathBuf) {
        android_session_in(std::path::Path::new("/tmp"), tag)
    }

    fn android_session_in(base: &std::path::Path, tag: &str) -> (Session, std::path::PathBuf) {
        let root = base.join(format!("sim-api-android-{tag}-{}", std::process::id()));
        let sdk = root.join("sdk");
        for (dir, image_tag) in [
            ("android-36/google_apis/arm64-v8a", "google_apis"),
            ("android-34/android-wear/arm64-v8a", "android-wear"),
            ("android-34/google-xr/arm64-v8a", "android-xr"),
        ] {
            let d = sdk.join("system-images").join(dir);
            std::fs::create_dir_all(&d).expect("image dir");
            std::fs::write(d.join("system.img"), b"s").expect("system.img");
            std::fs::write(d.join("source.properties"), format!("AndroidVersion.ApiLevel=36\nSystemImage.TagId={image_tag}\n"))
                .expect("props");
        }
        let env = emu_avd::Env { sdk_root: Some(sdk), avd_home: root.join("avd") };
        let mut android = sim_android::Android::new(env);
        android.running_dir = Some(root.join("running"));
        (Session::with_android(android), root)
    }

    #[test]
    fn android_devices_are_created_listed_and_routed() {
        let (mut s, root) = android_session("route");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        let fold =
            call(&mut s, "create", &json!({"platform": "android", "name": "fold", "system_image": mobile, "form_factor": "foldable"}))
                .expect("foldable");
        assert_eq!((fold["platform"].as_str(), fold["form_factor"].as_str()), (Some("android"), Some("foldable")));
        let wear = "system-images;android-34;android-wear;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "watch", "system_image": wear})).expect("watch");
        let err = call(&mut s, "create", &json!({"platform": "android", "name": "t", "system_image": wear, "form_factor": "tablet"}));
        assert!(err.expect_err("tablet from Wear").contains("boots Wear OS · Watch devices, not tablet"));
        let err = call(&mut s, "create", &json!({"platform": "android", "name": "v", "system_image": mobile, "form_factor": "vision"}));
        assert!(err.expect_err("vision from a phone image").ends_with("boots Android · Phone devices, not vision"));

        let devices = call(&mut s, "devices", &json!({"platform": "android"})).expect("devices");
        let labels: Vec<(&str, &str)> = devices
            .as_array()
            .expect("array")
            .iter()
            .map(|d| (d["udid"].as_str().unwrap_or(""), d["sub_platform"].as_str().unwrap_or("")))
            .collect();
        assert_eq!(labels, [("fold", "android"), ("watch", "wearos")]);
        // Android only: a macOS host lists its real Apple Watch simulators as watches too.
        let watches = call(&mut s, "devices", &json!({"form_factor": "watch", "platform": "android"})).expect("watches");
        assert_eq!(watches.as_array().map(Vec::len), Some(1));
        assert_eq!(call(&mut s, "devices", &json!({"booted": true, "platform": "android"})).expect("booted"), json!([]));
        let rts = call(&mut s, "runtimes", &json!({"platform": "android"})).expect("runtimes");
        assert_eq!(rts.as_array().map(Vec::len), Some(3));

        assert_eq!(call(&mut s, "screen", &json!({"device": "watch"})).expect("screen"), json!({"width": 454, "height": 454}));
        let info = call(&mut s, "info", &json!({"device": "fold"})).expect("info");
        assert_eq!(info["profile"]["fold"]["angle"], 180.0);
        let err = call(&mut s, "status_bar", &json!({"device": "watch"})).expect_err("iOS-only");
        assert_eq!(err, "`status_bar` isn't available on Android devices (it works on iOS)");
        let err = call(&mut s, "fold", &json!({"device": "watch", "posture": "closed"})).expect_err("not a foldable");
        assert!(err.starts_with("folding needs a device with Hinge; watch is Wear OS · Watch"), "{err}");
        assert_eq!(
            call(&mut s, "fold", &json!({"device": "fold", "posture": "ajar"})).expect_err("posture"),
            "unknown posture `ajar` (closed, half-opened, opened, flipped, tent)"
        );
        assert_eq!(call(&mut s, "rotate", &json!({"device": "fold"})).expect_err("stopped"), "fold isn't running; boot it first");
        let err = call(&mut s, "rotate", &json!({"device": "fold", "orientation": "portrait"})).expect_err("iOS-only orientation");
        assert!(err.contains("`orientation` is iOS-only"), "{err}");
        let err = call(&mut s, "tap", &json!({"device": "fold", "x": 1, "y": 1, "display": "carplay"})).expect_err("no CarPlay");
        assert!(err.starts_with("display=carplay is an iPhone's CarPlay screen"), "{err}");
        let err = call(&mut s, "carplay", &json!({"device": "fold", "on": true})).expect_err("iOS-only");
        assert_eq!(err, "`carplay` isn't available on Android devices (it works on iOS)");
        // `platform` keeps the lookup off the simulator service, whose first start on a fresh
        // runner can outlast the test (#110).
        let missing = json!({"device": "no-such-device", "platform": "android", "x": 1, "y": 1});
        assert_eq!(call(&mut s, "tap", &missing).expect_err("missing"), "no device matches `no-such-device`");
        call(&mut s, "delete", &json!({"device": "watch"})).expect("delete");
        assert_eq!(call(&mut s, "devices", &json!({"platform": "android"})).expect("devices").as_array().map(Vec::len), Some(1));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn booted_resolves_among_android_devices_by_what_runs() {
        let (mut s, root) = short_android_session("booted");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        for name in ["a", "b", "c"] {
            call(&mut s, "create", &json!({"platform": "android", "name": name, "system_image": mobile})).expect("create");
        }
        let booted = json!({"device": "booted", "platform": "android"});
        assert_eq!(resolve(&mut s, &booted), Err("no device matches `booted`".into()), "AVDs exist, none runs");
        let android = s.android.backend().clone();
        let _a = sim_android::testing::fake_emulator(&android, "a");
        assert_eq!(resolve(&mut s, &booted), Ok((Platform::Android, "a".into())));
        assert_eq!(resolve(&mut s, &json!({"device": "c", "platform": "android"})), Ok((Platform::Android, "c".into())));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_simulator_udid_is_routed_without_listing_either_platform() {
        let (mut s, root) = android_session("udid");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "phone", "system_image": mobile})).expect("phone");
        // With an AVD around, a name is resolved across both platforms; a UDID goes straight to
        // the iOS backend, which looks it up (here it would find none: the routing is the test).
        let udid = "0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000";
        assert_eq!(resolve(&mut s, &json!({"device": udid})), Ok((Platform::Ios, udid.to_owned())));
        // An AVD whose name has a UDID's shape is still that AVD, and `platform` still decides.
        let avd = "1111aaaa-bbbb-cccc-dddd-eeeeffff1111";
        call(&mut s, "create", &json!({"platform": "android", "name": avd, "system_image": mobile})).expect("uuid-named avd");
        assert_eq!(resolve(&mut s, &json!({"device": avd})), Ok((Platform::Android, avd.to_owned())));
        assert!(resolve(&mut s, &json!({"device": udid, "platform": "android"})).is_err(), "no such AVD");
        // A mistyped UDID with AVDs around says "no device matches", not a platform mismatch.
        let missing = format!("no device matches `{udid}`");
        let err = call(&mut s, "boot_many", &json!({"devices": ["phone", udid]})).expect_err("mixed");
        assert_eq!(err, missing);
        let err = call(&mut s, "boot_many", &json!({"devices": [udid, "phone"]})).expect_err("mixed");
        assert_eq!(err, missing);
        assert_eq!(call(&mut s, "xr", &json!({"device": udid})).expect_err("no such device"), missing);
        assert_eq!(call(&mut s, "console", &json!({"device": udid})).expect_err("no such device"), missing);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn running_android_devices_are_driven_through_every_method() {
        use base64::Engine;
        use sim_android::testing::{fake_emulator, wait_for};
        use sim_core::form_factor::FormFactor;
        // Under /tmp, not $TMPDIR: the fake display socket's path must fit a unix socket address
        // (104 bytes on macOS).
        let root = std::path::PathBuf::from("/tmp").join(format!("sa-live-{}", std::process::id()));
        let sdk = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&sdk).expect("image dir");
        std::fs::write(sdk.join("system.img"), b"s").expect("system.img");
        std::fs::write(sdk.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let env = emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") };
        let mut android = sim_android::Android::new(env);
        android.running_dir = Some(root.join("running"));
        android.create("fold", "system-images;android-36;google_apis;arm64-v8a", Some(FormFactor::Foldable)).expect("create");
        let fake = fake_emulator(&android, "fold");
        let mut s = Session::with_android(android);
        let ok = |s: &mut Session, method: &str, params: Value| call(s, method, &params).unwrap_or_else(|e| panic!("{method}: {e}"));

        ok(&mut s, "tap", json!({"device": "fold", "x": 10, "y": 20}));
        ok(&mut s, "touch", json!({"device": "fold", "x": 5, "y": 6, "phase": "down"}));
        ok(&mut s, "button", json!({"device": "fold", "button": "home"}));
        ok(&mut s, "key", json!({"device": "fold", "usage": 0x28}));
        ok(&mut s, "location", json!({"device": "fold", "latitude": 37.5, "longitude": -122.25}));
        ok(&mut s, "rotate", json!({"device": "fold"}));
        assert_eq!(ok(&mut s, "fold", json!({"device": "fold", "posture": "half-opened"}))["posture"], "half-opened");
        assert_eq!(ok(&mut s, "fold", json!({"device": "fold", "angle": 170}))["angle"], 170.0);
        assert_eq!(ok(&mut s, "console", json!({"device": "fold", "command": "ping"})), json!(["I am alive!"]));
        assert_eq!(ok(&mut s, "console", json!({"device": "fold", "commands": ["frames", "ping"]})), json!(["3", "I am alive!"]));
        assert_eq!(call(&mut s, "console", &json!({"device": "fold"})).expect_err("no command"), "give `command` or `commands`");

        let shot = ok(&mut s, "screenshot", json!({"device": "fold"}));
        assert_eq!((shot["width"].as_u64(), shot["height"].as_u64()), (Some(2), Some(1)));
        let png = base64::engine::general_purpose::STANDARD.decode(shot["png"].as_str().expect("png")).expect("base64");
        assert_eq!(sim_core::frame::png_size(&png), Some((2, 1)), "the emulator's PNG, passed through");
        let path = root.join("shot.png");
        let saved = ok(&mut s, "screenshot", json!({"device": "fold", "path": path.to_str()}));
        assert_eq!(saved["width"], 2);
        assert!(std::fs::read(&path).expect("written by the emulator").starts_with(b"\x89PNG"));

        let console = wait_for(&fake.console, 13);
        for want in
            ["event send EV_KEY:KEY_HOME:1", "event send EV_KEY:KEY_HOME:0", "event send EV_KEY:28:1", "geo fix -122.25 37.5", "rotate"]
        {
            assert!(console.iter().any(|l| l == want), "{want} not in {console:?}");
        }
        assert!(console.iter().any(|l| l.starts_with("sensor set hinge-angle0 ")), "{console:?}");
        assert!(console.iter().any(|l| l == "posture 2") && console.iter().any(|l| l == "fold status"), "postures as stock: {console:?}");
        assert_eq!(wait_for(&fake.display, 3)[..3], ["touch 10 20 down", "touch 10 20 up", "touch 5 6 down"]);

        // Form-factor checks happen before anything reaches the emulator.
        let err = |s: &mut Session, method: &str, params: Value| call(s, method, &params).expect_err(method);
        assert!(err(&mut s, "rotary", json!({"device": "fold", "ticks": 1})).starts_with("rotary input needs a device with Rotary"));
        assert!(err(&mut s, "vehicle", json!({"device": "fold"})).contains("vehicle HAL"));
        assert!(err(&mut s, "touchpad", json!({"device": "fold", "x": 1, "y": 1})).contains("no touchpad"));
        assert_eq!(err(&mut s, "rotary", json!({"device": "fold"})), "missing number parameter `ticks`");
        assert_eq!(err(&mut s, "key", json!({"device": "fold"})), "missing number parameter `usage`");
        assert!(err(&mut s, "fold", json!({"device": "fold"})).starts_with("give `folded`"));
        assert!(err(&mut s, "fold", json!({"device": "fold", "folded": null})).starts_with("give `folded`"), "null is absent");
        assert_eq!(err(&mut s, "fold", json!({"device": "fold", "folded": "yes"})), "`folded` must be true or false, not \"yes\"");
        assert!(err(&mut s, "rotate", json!({"device": "fold", "orientation": "portrait"})).contains("iOS-only"));
        std::fs::remove_dir_all(root).ok();
    }

    /// One running Pixel Fold AVD named `fold` on a fake emulator (stock's or simee's), under /tmp
    /// (the fake display socket's path must fit a unix socket address).
    fn running_foldable(tag: &str, stock: bool) -> (Session, sim_android::testing::FakeEmulator, std::path::PathBuf) {
        let root = std::path::PathBuf::from("/tmp").join(format!("sa-{tag}-{}", std::process::id()));
        let sdk = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&sdk).expect("image dir");
        std::fs::write(sdk.join("system.img"), b"s").expect("system.img");
        std::fs::write(sdk.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let env = emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") };
        let mut android = sim_android::Android::new(env);
        android.running_dir = Some(root.join("running"));
        android.create_device("fold", "system-images;android-36;google_apis;arm64-v8a", None, Some("pixel_fold")).expect("create");
        let fake = match stock {
            true => sim_android::testing::fake_stock_emulator(&android, "fold"),
            false => sim_android::testing::fake_emulator(&android, "fold"),
        };
        (Session::with_android(android), fake, root)
    }

    #[test]
    fn a_stock_run_foldable_folds_without_simees_status() {
        let (mut s, fake, root) = running_foldable("stockfold", true);
        let fold = |s: &mut Session, p: Value| call(s, "fold", &p);
        // Stock refuses simee's `fold status` (`KO: Usage: "fold"`): the answer is what was asked.
        assert_eq!(fold(&mut s, json!({"device": "fold", "folded": true})), Ok(json!({"folded": true})));
        assert_eq!(fold(&mut s, json!({"device": "fold", "posture": "opened"})).expect("posture")["posture"], "opened");
        assert_eq!(fold(&mut s, json!({"device": "fold", "angle": 90})).expect("angle")["angle"], 90.0);
        let console = sim_android::testing::wait_for(&fake.console, 3);
        assert_eq!(console, ["fold", "posture 3", "sensor set hinge-angle0 90"], "stock's own commands only");
        assert_eq!(call(&mut s, "screen", &json!({"device": "fold"})), Ok(json!({"width": 2208, "height": 1840})));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_folded_foldable_takes_points_in_the_guests_layout() {
        use sim_android::testing::wait_for;
        let (mut s, fake, root) = running_foldable("folded", false);
        let ok = |s: &mut Session, method: &str, params: Value| call(s, method, &params).unwrap_or_else(|e| panic!("{method}: {e}"));
        // Unfolded (simee's status: the whole screen), points pass through.
        *fake.fold_status.lock().expect("status") = Some(
            "angle: 180\r\nposture: opened (3)\r\nfolded: false\r\nvisible: 2208x1840+0+0\r\nsize: 2208x1840\r\npostures: 1,2,3".into(),
        );
        assert_eq!(ok(&mut s, "screen", json!({"device": "fold"})), json!({"width": 2208, "height": 1840}));
        ok(&mut s, "touch", json!({"device": "fold", "x": 1000, "y": 1000, "phase": "down"}));
        assert_eq!(wait_for(&fake.display, 1), ["touch 1000 1000 down"]);
        // Folded, the guest lays out for the outer screen (uiautomator's bounds) while the frames
        // and the display socket's touches are its 949x1840 region of the scanout.
        *fake.fold_status.lock().expect("status") =
            Some("angle: 0\r\nposture: closed (1)\r\nfolded: true\r\nvisible: 949x1840+629+0\r\nsize: 1080x2092\r\npostures: 1,2,3".into());
        assert_eq!(ok(&mut s, "screen", json!({"device": "fold"})), json!({"width": 1080, "height": 2092}));
        ok(&mut s, "tap", json!({"device": "fold", "x": 1080, "y": 2092, "hold_ms": 0}));
        ok(&mut s, "touch", json!({"device": "fold", "x": 540, "y": 1046, "phase": "up"}));
        assert_eq!(
            wait_for(&fake.display, 4)[1..],
            ["touch 949 1840 down", "touch 949 1840 up", "touch 474 920 up"],
            "whole pixels on the wire"
        );
        std::fs::remove_dir_all(root).ok();
    }

    /// Per-call cost of a device method among many AVDs (the API resolves `device` on every call).
    ///     cargo test --release -p simee-api api_call_latency -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn api_call_latency() {
        use sim_android::testing::fake_emulator;
        let root = std::path::PathBuf::from("/tmp").join(format!("sa-bench-{}", std::process::id()));
        let sdk = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&sdk).expect("image dir");
        std::fs::write(sdk.join("system.img"), b"s").expect("system.img");
        std::fs::write(sdk.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let env = emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") };
        let mut android = sim_android::Android::new(env);
        android.running_dir = Some(root.join("running"));
        for i in 0..20 {
            android.create(&format!("avd{i:02}"), "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        }
        let _fake = fake_emulator(&android, "avd07");
        let mut s = Session::with_android(android);
        let n = 200;
        for (method, params) in
            [("screen", json!({"device": "avd07"})), ("touch", json!({"device": "avd07", "x": 1, "y": 2, "phase": "down"}))]
        {
            let start = std::time::Instant::now();
            for _ in 0..n {
                call(&mut s, method, &params).expect(method);
            }
            println!("api {method:<6}: {:.3} ms per call (20 AVDs)", start.elapsed().as_secs_f64() * 1000.0 / f64::from(n));
        }
        std::fs::remove_dir_all(root).ok();
    }

    /// An `adb` in the session's SDK that says every device has booted (no real adb runs).
    fn fake_adb(root: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let dir = root.join("sdk/platform-tools");
        std::fs::create_dir_all(&dir).expect("platform-tools");
        let adb = dir.join("adb");
        std::fs::write(&adb, "#!/bin/sh\necho 1\n").expect("adb");
        std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn boot_many_clone_settle_and_profile_work_on_both_platforms() {
        let (mut s, root) = android_session("boot-many");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "pixel", "system_image": mobile})).expect("pixel");
        // A listing resolves no device (it resolved the default `booted`, which fails with two booted).
        assert!(call(&mut s, "device_types", &Value::Null).is_ok_and(|t| t.is_array()));
        // Android's hardware is listed with or without a platform; asked for iOS, none.
        let android = call(&mut s, "device_types", &json!({"platform": "android"})).expect("android types");
        assert!(android.as_array().is_some_and(|a| !a.is_empty() && a.iter().all(|t| t["platform"] == "android")), "{android}");
        let ios = call(&mut s, "device_types", &json!({"platform": "ios"})).expect("ios types");
        assert!(ios.as_array().is_some_and(|a| a.iter().all(|t| t["platform"] == "ios")), "{ios}");
        let help = call(&mut s, "help", &Value::Null).expect("help");
        let row = |m: &str| help.as_array().and_then(|a| a.iter().find(|x| x["method"] == m)).cloned().expect(m);
        for m in ["boot_many", "clone", "settle", "profile", "device_types"] {
            assert_eq!(row(m)["platforms"], json!(["android", "ios"]), "{m}");
        }
        // Both need a running device.
        let err = call(&mut s, "settle", &json!({"device": "pixel"})).expect_err("not booted");
        assert_eq!(err, "pixel isn't booted");
        let err = call(&mut s, "profile", &json!({"device": "pixel", "name": "slim"})).expect_err("not booted");
        assert_eq!(err, "pixel isn't booted");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn android_boot_many_checks_its_parameters_before_starting_anything() {
        let (mut s, root) = android_session("boot-many-params");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "pixel", "system_image": mobile})).expect("pixel");
        let many = |extra: Value| {
            let mut p = json!({"devices": ["pixel"]});
            p.as_object_mut().expect("object").extend(extra.as_object().expect("object").clone());
            p
        };
        for (extra, want) in [
            (
                json!({"settle": true}),
                "`boot_many` takes `settle` for iOS devices only (an Android boot waits for sys.boot_completed; call `settle` after it",
            ),
            (
                json!({"profile": "slim"}),
                "`boot_many` takes `profile` for iOS devices only (on Android, call `profile` on the booted device)",
            ),
            (json!({"env": {"A": "1"}}), "`boot_many` takes `env` for iOS devices only"),
            (json!({"concurrency": 0}), "concurrency must be at least 1"),
            (json!({"first_port": 80}), "first_port must be a console port from 5554 to 5682"),
        ] {
            let err = call(&mut s, "boot_many", &many(extra.clone())).expect_err("refused");
            assert!(err.starts_with(want), "{extra}: {err}");
        }
        // An unknown name with no `platform` would list every simulator too, and the simulator
        // service's first start on a fresh runner can outlast the test (#110).
        let err = call(&mut s, "boot_many", &json!({"devices": ["pixel", "nope"], "platform": "android"})).expect_err("unknown");
        assert_eq!(err, "no device matches `nope`");
        let err = call(&mut s, "boot", &json!({"device": "pixel", "settle": true})).expect_err("iOS-only parameter");
        assert!(err.starts_with("`boot` takes `settle` for iOS devices only"), "{err}");
        let err = call(&mut s, "boot", &json!({"device": "pixel", "profile": "slim"})).expect_err("iOS-only parameter");
        assert!(err.starts_with("`boot` takes `profile` for iOS devices only"), "{err}");
        let err = call(&mut s, "boot", &json!({"device": "pixel", "audio": "yes"})).expect_err("not a boolean");
        assert!(err.starts_with("`audio` must be true or false"), "{err}");
        let boot = METHODS.iter().find(|m| m.0 == "boot").expect("boot");
        assert!(boot.1.contains("Android: audio?(false)"), "{}", boot.1);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn android_boot_many_reports_each_device_like_ios() {
        use sim_android::testing::fake_emulator;
        let (mut s, root) = short_android_session("bm-run");
        fake_adb(&root);
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "pixel", "system_image": mobile})).expect("pixel");
        // Running already: boot_many waits for it (console and adb answer) and starts nothing.
        let _fake = fake_emulator(s.android.backend(), "pixel");
        let r = call(&mut s, "boot_many", &json!({"devices": ["pixel"], "concurrency": 2, "timeout_s": 20})).expect("boot_many");
        assert_eq!((r["concurrency"].as_u64(), r["failed"].as_u64(), r["unsettled"].as_u64()), (Some(2), Some(0), Some(0)), "{r}");
        let d = &r["devices"][0];
        assert_eq!(d["device"], "pixel");
        let port = d["console_port"].as_u64().expect("port");
        assert_eq!(d["serial"], json!(format!("emulator-{port}")));
        assert!(d["boot_s"].as_f64().is_some_and(|s| s < 20.0), "{d}");
        assert!(d.get("error").is_none());
        std::fs::remove_dir_all(root).ok();
    }

    /// A host with `total_gib` of memory, 59% free, 16 CPUs at load 2, and its verdict (thermal
    /// `serious` holds it).
    fn fake_host(total_gib: u64, thermal: Option<sim_host::ThermalState>) -> HostState {
        use sim_host::{Level, MemReading, Reading, Thresholds, decide};
        let reading = Reading {
            load1: Some(2.0),
            cpus: 16,
            mem: Some(MemReading { available_bytes: None, level: Level::Normal }),
            thermal,
            power: None,
        };
        HostState {
            cpus: 16,
            load1: Some(2.0),
            memory_total_bytes: Some(total_gib << 30),
            memory_available_bytes: None,
            memory_free_pct: Some(59.0),
            now: decide(&reading, &Thresholds::default()),
            basis: None,
        }
    }

    const SERIOUS: Option<sim_host::ThermalState> = Some(sim_host::ThermalState::Serious);

    /// A session on an SDK and AVD home under `dir` holding an AVD `pixel`, on a host whose
    /// thermal state holds every boot: a `boot` with `admission: "wait"` waits until cancelled
    /// (`shutdown`'s server child, which makes one per connection on the same files).
    pub(crate) fn held_android_session(dir: &std::path::Path) -> Session {
        let (mut s, root) = android_session_in(dir, "held");
        fake_adb(&root);
        s.fake_host = Some(fake_host(64, SERIOUS));
        if !root.join("avd/pixel.ini").exists() {
            let mobile = "system-images;android-36;google_apis;arm64-v8a";
            call(&mut s, "create", &json!({"platform": "android", "name": "pixel", "system_image": mobile})).expect("pixel");
        }
        s
    }

    #[test]
    fn admission_check_predicts_what_new_devices_cost() {
        let mut s = Session { fake_host: Some(fake_host(64, None)), ..Session::default() };
        let a = call(&mut s, "admission_check", &json!({"platform": "android", "count": 3})).expect("check");
        assert_eq!(a["verdict"], "admit", "{a}");
        let p = &a["predicted"];
        assert_eq!(
            (p["subject"].as_str(), p["devices"].as_u64(), p["admissible"].as_u64()),
            (Some("3 more Android phone devices"), Some(3), Some(3))
        );
        assert_eq!(p["costs"][0]["count"], 3);
        assert_eq!(p["boot_memory_bytes"], 3 * 3_764_000_000u64);
        assert_eq!(p["memory_used_pct"], json!({"now": 41.0, "after": 57.4}));
        assert!(
            a["reasons"][0].as_str().is_some_and(|r| r.starts_with("3 more Android phone devices would take memory used from 41% to 57%"))
        );
        // Twelve phones overfill a 64 GiB host (107% used, a load of 32 against 24): the first 8 fit.
        let full = call(&mut s, "admission_check", &json!({"platform": "android", "count": 12})).expect("check");
        assert_eq!((full["verdict"].as_str(), full["predicted"]["admissible"].as_u64()), (Some("backoff"), Some(8)), "{full}");
        let watch = call(&mut s, "admission_check", &json!({"platform": "ios", "form_factor": "watch"})).expect("watch");
        assert!(watch["predicted"]["costs"][0]["source"].as_str().is_some_and(|x| x.contains("a watch isn't measured yet")), "{watch}");
        // The parameters are checked.
        let mut refused = |p: Value| call(&mut s, "admission_check", &p).expect_err("refused");
        assert_eq!(refused(json!({})), "give `platform`: ios or android");
        assert_eq!(refused(json!({"platform": "android", "count": 0})), "count must be a whole number from 1 to 1000");
        assert!(refused(json!({"platform": "android", "form_factor": "toaster"})).starts_with("unknown form factor `toaster`"));
        if let Some((f, pl, _)) = matrix().into_iter().find(|(_, _, k)| k.is_err()) {
            assert!(!refused(json!({"platform": pl, "form_factor": f})).is_empty());
        }
        // On the real host: a verdict and its reasons, whatever the host's state.
        let real = call(&mut Session::default(), "admission_check", &json!({"platform": "ios"})).expect("real host");
        assert!(matches!(real["verdict"].as_str(), Some("admit" | "hold" | "backoff")) && real["reasons"][0].is_string(), "{real}");
    }

    #[test]
    fn the_admission_modes() {
        use sim_host::admission::seed;
        let judged = |total_gib, thermal| {
            admit(&fake_host(total_gib, thermal), &[Item { platform: Platform::Android, form_factor: None }], 1, seed, &Limits::default())
        };
        let (admits, holds) = (judged(64, None), judged(64, SERIOUS));
        assert_eq!((admits.verdict, holds.verdict), (Verdict::Admit, Verdict::Hold));
        for mode in [Mode::Ignore, Mode::Wait] {
            assert_eq!((gate(mode, &admits), gate(mode, &holds)), (Ok(()), Ok(())), "{mode:?} never refuses");
        }
        assert_eq!(gate(Mode::Refuse, &admits), Ok(()));
        let err = gate(Mode::Refuse, &holds).expect_err("refused");
        assert!(
            err.starts_with("admission hold: thermal: ") && err.ends_with("(nothing was done; admission \"ignore\" goes ahead anyway)"),
            "{err}"
        );
        let mode = |m: &str, p: Value| admission_mode(m, &p);
        assert_eq!(mode("boot", json!({})), Ok(Mode::Ignore));
        assert_eq!(mode("boot_many", json!({"admission": "wait"})), Ok(Mode::Wait));
        assert_eq!(mode("clone", json!({"admission": "refuse"})), Ok(Mode::Refuse));
        assert_eq!(
            mode("create", json!({"admission": "wait"})),
            Err("`create` takes admission ignore or refuse; wait paces boots (`boot`, `boot_many`)".into())
        );
        assert_eq!(mode("boot", json!({"admission": "later"})), Err("unknown admission `later` (ignore, refuse or wait)".into()));
        assert_eq!(mode("boot", json!({"admission": true})), Err("admission must be ignore, refuse or wait, not true".into()));
        assert_eq!(wait_options(&json!({})).map(|w| w.timeout), Ok(Wait::DEFAULT_TIMEOUT));
        assert_eq!(wait_options(&json!({"admission_timeout_s": 7})).map(|w| w.timeout), Ok(Duration::from_secs(7)));
    }

    #[test]
    fn create_and_clone_return_admission_and_refuse_creates_nothing() {
        let (mut s, root) = short_android_session("adm-create");
        s.fake_host = Some(fake_host(64, SERIOUS));
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        let create = |extra: Value| {
            let mut p = json!({"platform": "android", "name": "fold", "system_image": mobile, "form_factor": "foldable"});
            p.as_object_mut().expect("object").extend(extra.as_object().cloned().unwrap_or_default());
            p
        };
        let err = call(&mut s, "create", &create(json!({"admission": "refuse"}))).expect_err("refused");
        assert!(err.starts_with("admission hold: thermal: "), "{err}");
        assert!(s.android.devices().is_empty(), "a refusal creates nothing");
        assert!(call(&mut s, "create", &create(json!({"admission": "wait"}))).is_err());
        // Ignore (the default) creates it and says what booting it would do.
        let d = call(&mut s, "create", &create(json!({}))).expect("created");
        assert_eq!((d["udid"].as_str(), d["form_factor"].as_str()), (Some("fold"), Some("foldable")));
        let a = &d["admission"];
        assert_eq!((a["verdict"].as_str(), a["predicted"]["costs"][0]["form_factor"].as_str()), (Some("hold"), Some("foldable")), "{a}");
        assert!(a["reasons"][0].as_str().is_some_and(|r| r.starts_with("thermal: ")), "{a}");
        // Clone judges the template's kind before it copies anything.
        let err = call(&mut s, "clone", &json!({"device": "fold", "name": "copy", "admission": "refuse"})).expect_err("refused");
        assert!(err.starts_with("admission hold: "), "{err}");
        s.fake_host = Some(fake_host(64, None));
        let err = call(&mut s, "clone", &json!({"device": "fold", "name": "copy", "admission": "refuse"})).expect_err("never booted");
        assert_eq!(err, "fold has no simee user data yet (boot it once with emu, then stop it)", "admitted: the clone itself answers");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn boot_and_boot_many_refuse_or_wait_on_a_busy_host() {
        use sim_android::testing::fake_emulator;
        let (mut s, root) = short_android_session("adm-boot");
        fake_adb(&root);
        s.fake_host = Some(fake_host(64, SERIOUS));
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "pixel", "system_image": mobile})).expect("pixel");
        let many = |extra: Value| {
            let mut p = json!({"devices": ["pixel"], "timeout_s": 20});
            p.as_object_mut().expect("object").extend(extra.as_object().cloned().unwrap_or_default());
            p
        };
        let err = call(&mut s, "boot_many", &many(json!({"admission": "refuse"}))).expect_err("refused");
        assert!(err.starts_with("admission hold: thermal: "), "{err}");
        let err = call(&mut s, "boot", &json!({"device": "pixel", "admission": "refuse"})).expect_err("refused");
        assert!(err.starts_with("admission hold: "), "{err}");
        let err = call(&mut s, "boot", &json!({"device": "pixel", "admission": "wait", "admission_timeout_s": 0})).expect_err("timed out");
        assert!(err.starts_with("admission: the host didn't admit the boot within 0 s (hold: thermal: "), "{err}");
        // Waiting past the timeout starts nothing and says so per device.
        let r = call(&mut s, "boot_many", &many(json!({"admission": "wait", "admission_timeout_s": 0}))).expect("paced");
        assert_eq!((r["failed"].as_u64(), r["pacing"]["timed_out"].as_bool()), (Some(1), Some(true)), "{r}");
        let e = r["devices"][0]["error"].as_str().expect("error");
        assert!(e.starts_with("not started: the host didn't admit it within 0 s of waiting (hold: thermal: "), "{e}");
        assert_eq!(r["pacing"]["steps"][0]["started"], 0);
        assert!(s.android.backend().device("pixel").expect("pixel").running.is_none(), "nothing was started");
        // A device that runs already costs nothing: admitted on the same busy host, at once.
        let _fake = fake_emulator(s.android.backend(), "pixel");
        let r = call(&mut s, "boot_many", &many(json!({"admission": "wait"}))).expect("running");
        assert_eq!((r["failed"].as_u64(), r["admission"]["verdict"].as_str()), (Some(0), Some("admit")), "{r}");
        assert_eq!((r["pacing"]["steps"][0]["started"].as_u64(), r["pacing"]["waited_s"].as_f64()), (Some(1), Some(0.0)), "{r}");
        assert_eq!(r["devices"][0]["device"], "pixel");
        let b = call(&mut s, "boot", &json!({"device": "pixel", "timeout_s": 20, "admission": "refuse"})).expect("running");
        assert_eq!((b["admission"]["verdict"].as_str(), b["admission"]["predicted"]["devices"].as_u64()), (Some("admit"), Some(0)), "{b}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_cancelled_wait_stops_promptly_boots_nothing_and_says_what_it_waited_on() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let (mut s, root) = short_android_session("adm-cancel");
        fake_adb(&root);
        s.fake_host = Some(fake_host(64, SERIOUS));
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "pixel", "system_image": mobile})).expect("pixel");
        let heard = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        // The caller cancels as soon as it hears what the call waits on.
        let hooks = || {
            let (flag, heard) = (Arc::new(AtomicBool::new(false)), heard.clone());
            let seen = flag.clone();
            CallHooks {
                cancelled: Box::new(move || seen.load(Ordering::Relaxed)),
                progress: Box::new(move |m| {
                    // Only what it waits on (a note counts nothing), not its steps.
                    if m.total.is_none() {
                        heard.lock().expect("heard").push(m.message.clone());
                        flag.store(true, Ordering::Relaxed);
                    }
                }),
            }
        };
        let t = std::time::Instant::now();
        let err =
            with_call_hooks(hooks(), || call(&mut s, "boot", &json!({"device": "pixel", "admission": "wait"}))).expect_err("cancelled");
        assert!(err.starts_with("admission: cancelled after waiting 0.") && err.ends_with("nothing was booted"), "{err}");
        let r =
            with_call_hooks(hooks(), || call(&mut s, "boot_many", &json!({"devices": ["pixel"], "timeout_s": 20, "admission": "wait"})))
                .expect("a result");
        assert!(t.elapsed() < Duration::from_secs(3), "stopped between polls, not at the 5 s poll or the 600 s timeout: {:?}", t.elapsed());
        let e = r["devices"][0]["error"].as_str().expect("error");
        assert!(e.starts_with("not started: cancelled while waiting"), "{e}");
        assert_eq!((r["failed"].as_u64(), r["pacing"]["timed_out"].as_bool()), (Some(1), Some(false)), "{r}");
        assert!(s.android.backend().device("pixel").expect("pixel").running.is_none(), "nothing was booted");
        let heard = heard.lock().expect("heard");
        assert!(heard.len() >= 2 && heard.iter().all(|m| m.contains("waiting at") && m.contains("hold: thermal: ")), "{heard:?}");
        // The hooks are gone after the call: a later call on this thread is not cancelled.
        assert!(!call_cancelled());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_failed_chunk_fails_its_devices_and_keeps_the_ones_already_booted() {
        let ok = json!({"devices": [{"device": "a", "boot_s": 1.0}], "failed": 0, "unsettled": 1});
        let ids = vec![vec!["a".to_owned()], vec!["b".to_owned(), "c".to_owned()]];
        let (devices, failed, unsettled) = merge_chunks(&[Ok(ok.clone()), Err("no console ports left".into())], &ids).expect("partial");
        assert_eq!((failed, unsettled), (2, 1));
        assert_eq!(devices[0]["device"], "a");
        assert_eq!(
            devices[1..]
                .iter()
                .map(|d| (d["device"].as_str(), d["error"].as_str().is_some_and(|e| e.contains("no console ports left"))))
                .collect::<Vec<_>>(),
            [(Some("b"), true), (Some("c"), true)]
        );
        // Every chunk failing is still the call's error; none started is an empty result.
        assert_eq!(merge_chunks(&[Err("plan".into()), Err("later".into())], &ids), Err("plan".into()));
        assert_eq!(merge_chunks(&[], &[]), Ok((vec![], 0, 0)));
    }

    /// A boot time in [`FakeFleet::ms`] that lasts until the fleet is `released`.
    const HOLD: u64 = u64::MAX;

    /// A fleet whose device `d<i>` takes `ms[i]` milliseconds to boot (none past the end; [`HOLD`]
    /// until `released`), booting `boot_many`'s devices in a sliding window as the backends do. It
    /// records each `boot_many` call's devices, each device it started and each it shut down.
    /// `live` are the devices running or booting now (another caller may add one at any time): a
    /// device's turn finds it live and waits on it, or finds it off and launches it, as the
    /// backends do.
    #[derive(Default)]
    struct FakeFleet {
        ms: Vec<u64>,
        released: std::sync::atomic::AtomicBool,
        calls: std::sync::Mutex<Vec<Vec<String>>>,
        begun: std::sync::Mutex<Vec<String>>,
        shut: std::sync::Mutex<Vec<String>>,
        live: std::sync::Mutex<std::collections::BTreeSet<String>>,
        /// How long a shutdown takes; one that takes any time releases the held boots first.
        shut_ms: u64,
        /// The answer to a `boot_many`, instead of booting its devices.
        fail: Option<String>,
        /// `(when, who)`: as `when` begins booting, another call brings `who` up.
        goes_live: Option<(&'static str, &'static str)>,
    }

    impl FakeFleet {
        fn with_live(self, ids: &[&str]) -> Self {
            self.live.lock().expect("live").extend(ids.iter().map(|id| (*id).to_owned()));
            self
        }

        /// Launches `id` unless it is live already (`ask` says whether it may), then waits for it.
        fn launch_and_boot(&self, id: &str, ask: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
            if !self.live.lock().expect("live").contains(id) {
                ask()?;
                self.live.lock().expect("live").insert(id.to_owned());
            }
            self.boot(id);
            Ok(())
        }

        fn boot(&self, id: &str) {
            self.begun.lock().expect("begun").push(id.to_owned());
            if let Some((_, who)) = self.goes_live.filter(|(when, _)| *when == id) {
                self.live.lock().expect("live").insert(who.to_owned());
            }
            let ms = id.strip_prefix('d').and_then(|n| n.parse::<usize>().ok()).and_then(|i| self.ms.get(i).copied()).unwrap_or(0);
            if ms == HOLD {
                while !self.released.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            } else {
                std::thread::sleep(Duration::from_millis(ms));
            }
        }
    }

    impl Booter for FakeFleet {
        fn run(&self, _: &str, p: &Value) -> Result<Value, String> {
            let id = str_param(p, "device").unwrap_or("").to_owned();
            self.launch_and_boot(&id, crate::progress::launching)?;
            Ok(json!({"devices": [{"device": id, "boot_s": 1.0}], "failed": 0, "unsettled": 0}))
        }

        fn boot_many(&self, p: &Value, hook: &dyn BootHook) -> Result<Value, String> {
            let ids = crate::params::strings(p, "devices");
            self.calls.lock().expect("calls").push(ids.clone());
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            let k = p["concurrency"].as_u64().unwrap_or(1) as usize;
            let next = std::sync::atomic::AtomicUsize::new(0);
            let results = std::sync::Mutex::new(vec![Value::Null; ids.len()]);
            let t0 = std::time::Instant::now();
            std::thread::scope(|scope| {
                for _ in 0..k.clamp(1, ids.len().max(1)) {
                    scope.spawn(|| {
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(id) = ids.get(i) else { break };
                            if let Err(why) = hook.start(i) {
                                results.lock().expect("results")[i] = json!({"device": id, "error": why});
                                continue;
                            }
                            let started_s = t0.elapsed().as_secs_f64();
                            let booted = self.launch_and_boot(id, || hook.launching(i));
                            let done_s = t0.elapsed().as_secs_f64();
                            let d = match booted {
                                Ok(()) => json!({"device": id, "boot_s": done_s - started_s, "started_s": started_s, "done_s": done_s}),
                                Err(why) => json!({"device": id, "error": why}),
                            };
                            hook.finished(i, &d);
                            results.lock().expect("results")[i] = d;
                        }
                    });
                }
            });
            let devices = results.into_inner().expect("results");
            let failed = devices.iter().filter(|d| d.get("error").is_some()).count();
            Ok(json!({"concurrency": k, "all_s": t0.elapsed().as_secs_f64(), "devices": devices, "failed": failed, "unsettled": 0}))
        }

        fn shut_down(&self, _: Platform, ids: &[String]) {
            if self.shut_ms > 0 {
                self.released.store(true, std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(self.shut_ms));
            }
            let mut live = self.live.lock().expect("live");
            for id in ids {
                live.remove(id);
            }
            self.shut.lock().expect("shut").extend(ids.iter().cloned());
        }
    }

    /// Android devices `d0`.. of which those in `running` run already.
    fn targets(n: usize, running: &[usize]) -> Vec<Target> {
        (0..n)
            .map(|i| Target {
                udid: format!("d{i}"),
                item: Item { platform: Platform::Android, form_factor: None },
                running: running.contains(&i),
            })
            .collect()
    }

    /// Hooks that record every step and cancel once `cancel_at` says so of a step.
    fn watching(
        heard: &std::sync::Arc<std::sync::Mutex<Vec<crate::Progress>>>,
        cancel_at: impl Fn(&crate::Progress) -> bool + 'static,
    ) -> (CallHooks, std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>) {
        let cancelled_at = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (heard, at, seen) = (heard.clone(), cancelled_at.clone(), cancelled_at.clone());
        let hooks = CallHooks {
            cancelled: Box::new(move || seen.lock().expect("at").is_some()),
            progress: Box::new(move |p| {
                heard.lock().expect("heard").push(p.clone());
                let mut at = at.lock().expect("at");
                if at.is_none() && cancel_at(p) {
                    *at = Some(std::time::Instant::now());
                }
            }),
        };
        (hooks, cancelled_at)
    }

    fn counts(heard: &std::sync::Mutex<Vec<crate::Progress>>) -> Vec<(String, u64, Option<u64>)> {
        heard.lock().expect("heard").iter().map(|p| (p.phase.clone(), p.done, p.total)).collect()
    }

    #[test]
    fn a_boot_many_refills_its_window_as_each_device_finishes_watched_or_not() {
        // d0 takes 0.9 s, d1..d5 0.15 s each, two at a time: in one window d1..d5 boot one after
        // another beside d0 (all of it ~0.9 s); in waves of two it would take 0.9 + 0.15 + 0.15 s.
        let ms = vec![900, 150, 150, 150, 150, 150];
        for watched in [false, true] {
            let fleet = std::sync::Arc::new(FakeFleet { ms: ms.clone(), ..Default::default() });
            let heard = std::sync::Arc::default();
            let mut tally = Tally { done: 0, failed: 0, total: 6 };
            let run = |tally: &mut Tally| boot_window(&fleet, &json!({}), &targets(6, &[]), 2, tally);
            let r =
                if watched { with_call_hooks(watching(&heard, |_| false).0, || run(&mut tally)) } else { run(&mut tally) }.expect("booted");
            assert_eq!(fleet.calls.lock().expect("calls").len(), 1, "one window for every device (watched: {watched})");
            let devices = r["devices"].as_array().expect("devices");
            assert_eq!(
                devices.iter().map(|d| d["device"].as_str().unwrap_or("")).collect::<Vec<_>>(),
                ["d0", "d1", "d2", "d3", "d4", "d5"]
            );
            let at = |i: usize, k: &str| devices[i][k].as_f64().expect("time");
            // In a window, d2 takes d1's slot the moment d1 finishes, while d0 still boots; in waves
            // it would wait for d0 too. (d5 isn't used: a loaded runner stretched the 0.15 s boots
            // to 0.3 s, so five of them outlast d0's 0.9 s even in a window.)
            assert!(at(2, "started_s") < at(0, "done_s"), "d2 started as d1 finished, not after d0 (watched: {watched}): {r}");
            assert!(at(2, "started_s") - at(1, "done_s") < 0.2, "d2 started when d1 finished (watched: {watched}): {r}");
            // The window property is the line above (in waves, d5 would start only after d0 is
            // done). An absolute wall-clock bound isn't asserted: a loaded CI runner stretched d0's
            // 0.9 s to 1.02 s and the whole run past any tight bound.
            assert_eq!((r["failed"].as_u64(), r.get("cancelled")), (Some(0), None), "{r}");
            assert_eq!((tally.done, tally.failed), (6, 0));
            let steps = counts(&heard);
            if watched {
                // Each device done as it finishes, the count only rising, ending at all of them.
                assert!(steps.iter().all(|s| s.0 == "booted" && s.2 == Some(6)), "{steps:?}");
                assert!(steps.windows(2).all(|w| w[0].1 < w[1].1) && steps.last().map(|s| s.1) == Some(6), "{steps:?}");
                assert!(steps.len() >= 3, "heard while it ran, not only at the end: {steps:?}");
            } else {
                assert!(steps.is_empty(), "nobody to tell: {steps:?}");
            }
            assert!(fleet.shut.lock().expect("shut").is_empty());
        }
        // A backend that refuses the call (a parameter) makes it the call's error.
        let refused =
            std::sync::Arc::new(FakeFleet { fail: Some("`boot_many` takes `settle` for iOS devices only".into()), ..Default::default() });
        let heard = std::sync::Arc::default();
        let (hooks, _) = watching(&heard, |_| false);
        let err =
            with_call_hooks(hooks, || boot_window(&refused, &json!({}), &targets(4, &[]), 2, &mut Tally { done: 0, failed: 0, total: 4 }));
        assert_eq!(err.expect_err("refused"), "`boot_many` takes `settle` for iOS devices only");
    }

    #[test]
    fn a_cancelled_boot_many_returns_the_devices_done_and_shuts_down_the_ones_it_was_booting() {
        // Ten devices four at a time: d0..d3 take 50 ms, the rest hold until released; d5 was
        // running already. Cancelled at 300 ms: d0..d3 are done, d4..d7 booting, d8 and d9 not
        // started.
        let fleet = std::sync::Arc::new(
            FakeFleet { ms: vec![50, 50, 50, 50, HOLD, HOLD, HOLD, HOLD, HOLD, HOLD], ..Default::default() }.with_live(&["d5"]),
        );
        let heard = std::sync::Arc::default();
        let (hooks, cancelled_at) = watching(&heard, |_| false);
        let at = cancelled_at.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            *at.lock().expect("at") = Some(std::time::Instant::now());
        });
        let mut tally = Tally { done: 0, failed: 0, total: 10 };
        let r = with_call_hooks(hooks, || boot_window(&fleet, &json!({}), &targets(10, &[5]), 4, &mut tally)).expect("partial");
        let took = cancelled_at.lock().expect("at").expect("cancelled").elapsed();
        assert!(took < Duration::from_secs(1), "stopped {took:?} after the cancel");
        assert_eq!((r["cancelled"].as_bool(), r["failed"].as_u64()), (Some(true), Some(6)), "{r}");
        let devices = r["devices"].as_array().expect("devices");
        assert!(devices[..4].iter().all(|d| d.get("error").is_none() && d["boot_s"].is_number()), "the four done are returned: {r}");
        let errors: Vec<&str> = devices[4..].iter().map(|d| d["error"].as_str().unwrap_or("")).collect();
        assert!(errors[0].contains("shut down") && errors[1].contains("left as it was"), "{errors:?}");
        assert!(errors[2].contains("shut down") && errors[3].contains("shut down"), "{errors:?}");
        assert_eq!(errors[4..], ["not started: boot_many was cancelled"; 2]);
        // The ones this call started that were still booting are shut down at once; d5 isn't touched.
        assert_eq!(*fleet.shut.lock().expect("shut"), ["d4", "d6", "d7"]);
        let last = heard.lock().expect("heard").last().cloned().expect("a step");
        assert_eq!((last.phase.as_str(), last.done, last.total), ("cancelled", 4, Some(10)));
        assert_eq!(last.message, "cancelled with 4 of 10 done; shut down 3 it was booting");
        // Released, the window starts nothing more, and its devices that come up after the cancel
        // are shut down again (a shutdown that came before the boot had begun would miss them).
        fleet.released.store(true, std::sync::atomic::Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while fleet.shut.lock().expect("shut").len() < 6 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut late = fleet.shut.lock().expect("shut")[3..].to_vec();
        late.sort();
        assert_eq!(late, ["d4", "d6", "d7"]);
        std::thread::sleep(Duration::from_millis(50));
        let mut begun = fleet.begun.lock().expect("begun").clone();
        begun.sort();
        assert_eq!(begun, ["d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7"], "d8 and d9 never started");
        assert_eq!(fleet.calls.lock().expect("calls").len(), 1);
    }

    /// Hooks that record every step and cancel `ms` milliseconds from now.
    fn cancelling_after(heard: &std::sync::Arc<std::sync::Mutex<Vec<crate::Progress>>>, ms: u64) -> CallHooks {
        let (hooks, cancelled_at) = watching(heard, |_| false);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            *cancelled_at.lock().expect("at") = Some(std::time::Instant::now());
        });
        hooks
    }

    /// Waits until `fleet` has shut down `n` devices (or 10 s), then a little longer for any more.
    fn shut_after_release(fleet: &FakeFleet, n: usize) -> Vec<String> {
        fleet.released.store(true, std::sync::atomic::Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while fleet.shut.lock().expect("shut").len() < n && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(100));
        let mut shut = fleet.shut.lock().expect("shut").clone();
        shut.sort();
        shut
    }

    #[test]
    fn a_cancelled_boot_many_never_shuts_down_a_device_another_call_started() {
        // d0 (50 ms) and d1 this call launches; d2 was off when it was admitted, but another call
        // boots it as d0 begins, so before this call's window reaches it (as d0 finishes).
        // Cancelled at 200 ms, waiting on d1 and d2: only d1, which this call booted, is shut down.
        let fleet = std::sync::Arc::new(FakeFleet { ms: vec![50, HOLD, HOLD], goes_live: Some(("d0", "d2")), ..Default::default() });
        let heard = std::sync::Arc::default();
        let hooks = cancelling_after(&heard, 200);
        let mut tally = Tally { done: 0, failed: 0, total: 3 };
        let r = with_call_hooks(hooks, || boot_window(&fleet, &json!({}), &targets(3, &[]), 2, &mut tally)).expect("partial");
        let errors: Vec<&str> = r["devices"].as_array().expect("devices").iter().map(|d| d["error"].as_str().unwrap_or("")).collect();
        assert_eq!(errors[0], "", "{r}");
        assert!(errors[1].contains("shut down") && errors[2].contains("left as it was"), "{errors:?}");
        assert_eq!(*fleet.shut.lock().expect("shut"), ["d1"]);
        let last = heard.lock().expect("heard").last().cloned().expect("a step");
        assert_eq!(last.message, "cancelled with 1 of 3 done; shut down 1 it was booting");
        // Both come up later: d1 is shut down again, d2 never.
        assert_eq!(shut_after_release(&fleet, 2), ["d1", "d1"]);
        assert!(fleet.live.lock().expect("live").contains("d2"));
    }

    #[test]
    fn a_cancelled_boot_many_counts_done_what_its_devices_say_at_the_cancel() {
        // Both still booting at the cancel; the shutdown takes 200 ms, and both come up during it.
        // The final step counts what `devices` says: none done.
        let fleet = std::sync::Arc::new(FakeFleet { ms: vec![HOLD, HOLD], shut_ms: 200, ..Default::default() });
        let heard = std::sync::Arc::default();
        let hooks = cancelling_after(&heard, 100);
        let mut tally = Tally { done: 0, failed: 0, total: 2 };
        let r = with_call_hooks(hooks, || boot_window(&fleet, &json!({}), &targets(2, &[]), 2, &mut tally)).expect("partial");
        let devices = r["devices"].as_array().expect("devices");
        assert!(devices.iter().all(|d| d["error"].as_str().is_some_and(|e| e.contains("shut down"))), "{r}");
        assert_eq!(r["failed"].as_u64(), Some(2));
        let last = heard.lock().expect("heard").last().cloned().expect("a step");
        assert_eq!((last.phase.as_str(), last.done, last.total), ("cancelled", 0, Some(2)));
        assert_eq!(last.message, "cancelled with 0 of 2 done; shut down 2 it was booting");
        assert_eq!((tally.done, tally.failed), (0, 0));
    }

    #[test]
    fn a_watched_boot_cancelled_while_another_call_boots_its_device_leaves_it_up() {
        // d0 was off when this `boot` was admitted; another call booted it since. Cancelled while
        // this call waits on it: it isn't shut down, then or once it comes up.
        let fleet = std::sync::Arc::new(FakeFleet { ms: vec![HOLD], ..Default::default() }.with_live(&["d0"]));
        let heard = std::sync::Arc::default();
        let hooks = cancelling_after(&heard, 100);
        let err =
            with_call_hooks(hooks, || boot_one(&fleet, json!({"device": "d0"}), "d0".into(), Platform::Android)).expect_err("cancelled");
        assert_eq!(err, "boot was cancelled while it waited on d0, which this call hadn't booted (another had), so it was left as it was");
        assert!(fleet.shut.lock().expect("shut").is_empty());
        assert_eq!(shut_after_release(&fleet, 1), Vec::<String>::new());
    }

    #[test]
    fn a_watched_boot_cancelled_mid_boot_shuts_down_the_device_it_started() {
        let fleet = std::sync::Arc::new(FakeFleet { ms: vec![HOLD], ..Default::default() });
        let heard = std::sync::Arc::default();
        // Cancelled 100 ms into the boot.
        let (hooks, cancelled_at) = watching(&heard, |_| false);
        let at = cancelled_at.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            *at.lock().expect("at") = Some(std::time::Instant::now());
        });
        let err =
            with_call_hooks(hooks, || boot_one(&fleet, json!({"device": "d0"}), "d0".into(), Platform::Android)).expect_err("cancelled");
        assert!(cancelled_at.lock().expect("at").expect("cancelled").elapsed() < Duration::from_secs(1));
        assert_eq!(err, "boot was cancelled while d0 was booting; this call had started it, so it was shut down");
        assert_eq!(*fleet.shut.lock().expect("shut"), ["d0"]);
        fleet.released.store(true, std::sync::atomic::Ordering::Relaxed);
        // Not cancelled: the boot's own result.
        let fleet = std::sync::Arc::new(FakeFleet::default());
        let (hooks, _) = watching(&heard, |_| false);
        let r = with_call_hooks(hooks, || boot_one(&fleet, json!({"device": "d1"}), "d1".into(), Platform::Android)).expect("booted");
        assert_eq!(r["devices"][0]["device"], "d1");
        assert!(fleet.shut.lock().expect("shut").is_empty());
    }

    #[test]
    fn watched_lifecycle_methods_report_their_phases_and_undo_a_cancelled_create() {
        let (mut s, root) = android_session("watched");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        let create = json!({"platform": "android", "name": "made", "system_image": mobile});
        let heard = std::sync::Arc::default();
        let (hooks, _) = watching(&heard, |_| false);
        let made = with_call_hooks(hooks, || call(&mut s, "create", &create)).expect("created");
        assert_eq!(made["udid"], "made");
        assert_eq!(counts(&heard), [("creating".into(), 0, Some(1)), ("created".into(), 1, Some(1))]);
        assert_eq!(heard.lock().expect("heard")[1].message, "created made");
        // Cancelled while the create ran (the flag is up after its first look): the device it made
        // is deleted and the call fails saying so.
        let looks = std::cell::Cell::new(0);
        let hooks = CallHooks {
            cancelled: Box::new(move || {
                looks.set(looks.get() + 1);
                looks.get() > 1
            }),
            progress: Box::new(|_| {}),
        };
        let again = json!({"platform": "android", "name": "half", "system_image": mobile});
        let err = with_call_hooks(hooks, || call(&mut s, "create", &again)).expect_err("cancelled");
        assert_eq!(err, "`create` was cancelled: it had made half, which was deleted");
        assert!(s.android.backend().device("half").is_err(), "deleted");
        // Install: checked, then installed; cancelled before it starts, nothing is done.
        heard.lock().expect("heard").clear();
        let apk = root.join("app.apk");
        std::fs::write(&apk, vec![0u8; 2_000_000]).expect("apk");
        let install = json!({"device": "made", "path": apk.to_str()});
        let (hooks, _) = watching(&heard, |p| p.phase == "checking");
        let err = with_call_hooks(hooks, || call(&mut s, "install", &install)).expect_err("cancelled");
        assert!(err.starts_with("`install` was cancelled before installing ") && err.ends_with("(2.0 MB); nothing was done"), "{err}");
        assert_eq!(counts(&heard), [("checking".into(), 0, Some(2))]);
        heard.lock().expect("heard").clear();
        let (hooks, _) = watching(&heard, |_| false);
        let err = with_call_hooks(hooks, || call(&mut s, "install", &install)).expect_err("not running");
        assert!(!err.is_empty());
        assert_eq!(counts(&heard), [("checking".into(), 0, Some(2)), ("installing".into(), 1, Some(2))], "no step for a failure");
        // Unwatched (the CLI), the same calls report nothing and behave as before.
        assert!(call(&mut s, "create", &again).is_ok());
        std::fs::remove_dir_all(root).ok();
    }

    /// Whether `v` fits `schema`, as far as [`crate::output_schema`] writes them (type, enum,
    /// properties, required, items, additionalProperties, anyOf): the path to the first misfit.
    fn misfit(schema: &Value, v: &Value, at: &str) -> Option<String> {
        if let Some(alts) = schema["anyOf"].as_array() {
            return if alts.iter().any(|a| misfit(a, v, at).is_none()) { None } else { Some(format!("{at}: none of anyOf fits {v}")) };
        }
        let kind = match v {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        let types: Vec<&str> = match &schema["type"] {
            Value::String(t) => vec![t.as_str()],
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        if !types.is_empty() && !types.contains(&kind) {
            return Some(format!("{at}: {kind} isn't {types:?}"));
        }
        if schema["enum"].as_array().is_some_and(|e| !e.contains(v)) {
            return Some(format!("{at}: {v} isn't one of {}", schema["enum"]));
        }
        match v {
            Value::Array(items) => items.iter().enumerate().find_map(|(i, item)| misfit(&schema["items"], item, &format!("{at}[{i}]"))),
            Value::Object(fields) => {
                let missing =
                    schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).find(|k| !fields.contains_key(*k));
                if let Some(k) = missing {
                    return Some(format!("{at}: no `{k}`"));
                }
                fields.iter().find_map(|(k, field)| {
                    let sub = schema["properties"].get(k).or_else(|| schema.get("additionalProperties"))?;
                    misfit(sub, field, &format!("{at}.{k}"))
                })
            }
            _ => None,
        }
    }

    /// What the methods return fits what their output schemas say, on fake Android devices and for
    /// the listings every host answers.
    #[test]
    fn results_fit_their_output_schemas() {
        use sim_android::testing::fake_emulator;
        let (mut s, root) = short_android_session("outputs");
        fake_adb(&root);
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        let created = call(&mut s, "create", &json!({"platform": "android", "name": "pixel", "system_image": mobile})).expect("pixel");
        let mut checked = vec![("create", created)];
        let _fake = fake_emulator(s.android.backend(), "pixel");
        let d = "pixel";
        for (method, p) in [
            ("help", json!({})),
            ("identity", json!({})),
            ("host", json!({})),
            ("devices_usage", json!({})),
            ("form_factors", json!({})),
            ("devices", json!({})),
            ("runtimes", json!({"platform": "android"})),
            ("screen", json!({"device": d})),
            ("screenshot", json!({"device": d})),
            ("contrast_check", json!({"device": d})),
            ("info", json!({"device": d})),
            ("apps", json!({"device": d})),
            ("launch", json!({"device": d, "bundle_id": "com.example.app"})),
            ("tap", json!({"device": d, "x": 1, "y": 2})),
            ("swipe", json!({"device": d, "x1": 1, "y1": 2, "x2": 3, "y2": 4, "duration_ms": 1})),
            ("type", json!({"device": d, "text": "a"})),
            ("button", json!({"device": d, "button": "back"})),
            ("location", json!({"device": d, "latitude": 1.0, "longitude": 2.0})),
            ("rotate", json!({"device": d})),
            ("console", json!({"device": d, "command": "help"})),
            ("boot_many", json!({"devices": [d], "timeout_s": 20})),
            ("boot_many", json!({"devices": [d], "timeout_s": 20, "admission": "wait"})),
            ("boot", json!({"device": d, "timeout_s": 20})),
            ("admission_check", json!({"platform": "android", "count": 2})),
        ] {
            if let Ok(v) = call(&mut s, method, &p) {
                checked.push((method, v));
            }
        }
        for (method, v) in &checked {
            let schema = crate::output_schema(method).expect(method);
            assert_eq!(misfit(&schema, v, method), None, "{method} returned {v}");
        }
        let names: Vec<&str> = checked.iter().map(|(m, _)| *m).collect();
        for method in [
            "help",
            "identity",
            "host",
            "devices_usage",
            "form_factors",
            "devices",
            "create",
            "screen",
            "info",
            "tap",
            "admission_check",
            "boot",
        ] {
            assert!(names.contains(&method), "{method} wasn't checked: {names:?}");
        }
        // The check can fail.
        assert!(misfit(&crate::output_schema("screen").expect("screen"), &json!({"width": "1"}), "screen").is_some());
        assert!(misfit(&crate::output_schema("tap").expect("tap"), &json!([]), "tap").is_some());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn boot_many_takes_one_platform_per_call() {
        let (mut s, root) = android_session("boot-many-mixed");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "pixel", "system_image": mobile})).expect("pixel");
        let ios = crate::ios::selection_infos(&mut s.ios).unwrap_or_default();
        if let Some(sim) = ios.first() {
            let err = call(&mut s, "boot_many", &json!({"devices": ["pixel", sim.udid]})).expect_err("mixed");
            assert_eq!(
                err,
                format!("`boot_many` boots one platform's devices per call: `pixel` is an Android device and `{}` an iOS one", sim.udid)
            );
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn android_clones_come_from_a_stopped_golden() {
        let (mut s, root) = short_android_session("clone");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "golden", "system_image": mobile})).expect("golden");
        let err = call(&mut s, "clone", &json!({"device": "golden", "name": "farm-1"})).expect_err("never booted");
        assert_eq!(err, "golden has no simee user data yet (boot it once with emu, then stop it)");
        // As if it had booted under simee once and stopped.
        let disks = emu_avd::state_dir(&root.join("avd/golden.avd"));
        std::fs::create_dir_all(&disks).expect("simee");
        std::fs::write(disks.join("userdata.img"), b"settled").expect("userdata");
        let clone = call(&mut s, "clone", &json!({"device": "golden", "name": "farm-1"})).expect("clone");
        assert_eq!((clone["udid"].as_str(), clone["platform"].as_str()), (Some("farm-1"), Some("android")));
        assert_eq!(std::fs::read(emu_avd::state_dir(&root.join("avd/farm-1.avd")).join("userdata.img")).expect("cloned"), b"settled");
        assert!(call(&mut s, "clone", &json!({"device": "golden"})).expect_err("no name").contains("name"));
        // A running golden is refused before anything is copied.
        let _fake = sim_android::testing::fake_emulator(s.android.backend(), "golden");
        let err = call(&mut s, "clone", &json!({"device": "golden", "name": "farm-2"})).expect_err("running");
        assert_eq!(err, "golden must be shut down to clone it");
        call(&mut s, "delete", &json!({"device": "farm-1"})).expect("delete the clone");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn android_devices_export_to_bundles_and_import_under_new_names() {
        let (mut s, root) = short_android_session("bundle");
        let sys = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::write(
            sys.join("source.properties"),
            "Pkg.Revision=7\nAndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\nSystemImage.Abi=arm64-v8a\n",
        )
        .expect("props");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "golden", "system_image": mobile})).expect("golden");
        let to = root.join("golden.tar");
        let err = call(&mut s, "export", &json!({"device": "golden", "to": to})).expect_err("never booted");
        assert!(err.contains("no simee user data"), "{err}");
        let disks = emu_avd::state_dir(&root.join("avd/golden.avd"));
        std::fs::create_dir_all(&disks).expect("simee");
        std::fs::write(disks.join("userdata.img"), b"settled").expect("userdata");
        let ex = call(&mut s, "export", &json!({"device": "golden", "to": to})).expect("export");
        assert_eq!(ex["summary"]["form"].as_str(), Some("tar"));
        assert_eq!(ex["identity"]["android"]["api"], "36");
        assert_eq!((ex["captures"]["ram"].as_bool(), ex["resume"].as_str()), (Some(false), Some("cold-boot")));
        let im = call(&mut s, "import", &json!({"from": to, "name": "copy"})).expect("import");
        assert_eq!((im["device"]["udid"].as_str(), im["device"]["platform"].as_str()), (Some("copy"), Some("android")));
        assert_eq!((im["exported_as"].as_str(), im["guest_identity"]["on_import"].as_str()), (Some("golden"), Some("duplicated")));
        assert_eq!((im["mismatch"].is_null(), im["dropped"].as_array().map(Vec::len)), (true, Some(0)));
        let bad = call(&mut s, "import", &json!({"from": to, "name": "copy3", "allow_mismatch": "yes"})).expect_err("bool");
        assert!(bad.contains("allow_mismatch"), "{bad}");
        assert_eq!(std::fs::read(emu_avd::state_dir(&root.join("avd/copy.avd")).join("userdata.img")).expect("imported"), b"settled");
        let err = call(&mut s, "import", &json!({"from": to, "name": "copy2", "platform": "ios"})).expect_err("ios");
        assert!(err.contains("Android bundles"), "{err}");
        // A running device is refused before anything is written.
        let _fake = sim_android::testing::fake_emulator(s.android.backend(), "golden");
        let err = call(&mut s, "export", &json!({"device": "golden", "to": root.join("again.tar")})).expect_err("running");
        assert_eq!(err, "golden must be shut down to export it");
        assert!(!root.join("again.tar").exists());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn android_xr_devices_take_xr_controls() {
        let (mut s, root) = android_session("xr");
        let xr = "system-images;android-34;google-xr;arm64-v8a";
        let headset = call(&mut s, "create", &json!({"platform": "android", "name": "headset", "system_image": xr})).expect("headset");
        assert_eq!((headset["form_factor"].as_str(), headset["sub_platform"].as_str()), (Some("vision"), Some("androidxr")));
        let wear = "system-images;android-34;android-wear;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "watch", "system_image": wear})).expect("watch");
        assert_eq!(call(&mut s, "screen", &json!({"device": "headset"})).expect("screen"), json!({"width": 2560, "height": 2558}));
        let visions = call(&mut s, "devices", &json!({"form_factor": "vision", "platform": "android"})).expect("visions");
        assert_eq!(visions.as_array().map(Vec::len), Some(1));

        let rotate = json!({"device": "headset", "action": "rotate", "x": 0, "y": 0.5, "z": 0});
        assert_eq!(call(&mut s, "xr", &rotate).expect_err("stopped"), "headset isn't running; boot it first");
        let err = call(&mut s, "xr", &json!({"device": "watch", "action": "recenter"})).expect_err("not XR");
        assert_eq!(err, "XR controls need an Android XR device; watch is Wear OS · Watch");
        let err = call(&mut s, "xr", &json!({"device": "headset", "action": "teleport"})).expect_err("unknown action");
        assert!(err.starts_with("xr: unknown variant `teleport`"), "{err}");
        let err = call(&mut s, "xr", &json!({"device": "headset", "action": "options", "environment": "day"})).expect_err("missing");
        assert!(err.contains("missing field `passthrough`"), "{err}");

        // The headset has no touchpad (the display glasses do); parameters are checked first.
        let mut pad = |p: Value| call(&mut s, "touchpad", &p);
        let err = pad(json!({"device": "headset", "x": 1, "y": 2})).expect_err("no touchpad");
        assert!(err.starts_with("headset has no touchpad"), "{err}");
        assert_eq!(pad(json!({"device": "headset", "x": 1})).expect_err("y"), "missing number parameter `y`");
        assert_eq!(pad(json!({"device": "headset", "x": -1, "y": 2})).expect_err("negative"), "`x` must be a whole number");
        assert_eq!(
            pad(json!({"device": "headset", "x": 1, "y": 2, "to_x": 3})).expect_err("half a swipe"),
            "a swipe needs both `to_x` and `to_y`"
        );
        assert_eq!(
            pad(json!({"device": "headset", "x": 1, "y": 2, "phase": "hold"})).expect_err("phase"),
            "unknown phase `hold` (down, move, up, tap)"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn vehicle_is_for_automotive_devices() {
        let (mut s, root) = android_session("vehicle");
        let car = root.join("sdk/system-images/android-34/android-automotive/arm64-v8a");
        std::fs::create_dir_all(&car).expect("image dir");
        std::fs::write(car.join("system.img"), b"s").expect("system.img");
        std::fs::write(car.join("source.properties"), "AndroidVersion.ApiLevel=34\nSystemImage.TagId=android-automotive\n").expect("props");
        let made = call(
            &mut s,
            "create",
            &json!({"platform": "android", "name": "car", "system_image": "system-images;android-34;android-automotive;arm64-v8a"}),
        )
        .expect("car");
        assert_eq!(made["form_factor"], "auto");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "phone", "system_image": mobile})).expect("phone");

        let err = call(&mut s, "vehicle", &json!({"device": "phone"})).expect_err("not a car");
        assert!(err.starts_with("the vehicle HAL needs a device with Automotive head unit; phone is Android · Phone"), "{err}");
        let err = call(&mut s, "vehicle", &json!({"device": "car", "property": "warp", "value": 1})).expect_err("unknown property");
        assert!(err.contains("unknown vehicle property `warp`"), "{err}");
        let err = call(&mut s, "vehicle", &json!({"device": "car", "property": "gear", "value": "fast"})).expect_err("bad value");
        assert!(err.contains("isn't a Int32 value for GEAR_SELECTION"), "{err}");
        assert_eq!(call(&mut s, "vehicle", &json!({"device": "car", "value": 1})).expect_err("no property"), "give the `property` to set");
        let err = call(&mut s, "vehicle", &json!({"device": "car", "property": "gear", "value": "drive"})).expect_err("stopped");
        assert_eq!(err, "car isn't running; boot it first");
        let err = call(&mut s, "vehicle", &json!({"device": "car", "property": "speed", "area_id": [1]})).expect_err("bad area");
        assert_eq!(err, "`area_id` must be a 32-bit number");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn form_factors_lists_the_matrix_with_reasons() {
        let mut s = Session::default();
        let all = call(&mut s, "form_factors", &Value::Null).expect("matrix");
        assert_eq!(all.as_array().map(Vec::len), Some(14));
        let fold = call(&mut s, "form_factors", &json!({"form_factor": "foldable"})).expect("foldable");
        let ios = fold.as_array().and_then(|a| a.iter().find(|x| x["platform"] == "ios")).expect("ios row");
        // The iPhone Duo: iOS's foldable, closed on its outer screen.
        assert_eq!((ios["supported"].as_bool(), ios["sub_platform"].as_str()), (Some(true), Some("ios")));
        assert_eq!(ios["profile"]["fold"]["folded_width"], 1398);
        let watch = call(&mut s, "form_factors", &json!({"form_factor": "watch", "platform": "ios"})).expect("watch");
        assert_eq!(watch[0]["sub_platform"], "watchos");
        assert_eq!(watch[0]["inputs"][1]["label"], "Digital Crown");
        assert_eq!(watch[0]["profile"]["shape"], "rounded");
        let xr = call(&mut s, "form_factors", &json!({"form_factor": "vision", "platform": "android"})).expect("Android XR");
        assert_eq!((xr[0]["supported"].as_bool(), xr[0]["sub_platform"].as_str()), (Some(true), Some("androidxr")));
        assert_eq!(xr[0]["inputs"][0]["label"], "Eye tracking");
        assert_eq!((xr[0]["profile"]["width"].as_u64(), xr[0]["profile"]["height"].as_u64()), (Some(2560), Some(2558)));
        assert!(all.as_array().expect("rows").iter().all(|r| r["supported"] == true), "every pair exists: {all}");
        let err = call(&mut s, "form_factors", &json!({"form_factor": "laptop"})).expect_err("unknown");
        assert!(err.contains("phone, tablet, foldable, watch, tv, auto, vision"), "{err}");
        assert!(call(&mut s, "devices", &json!({"platform": "windows"})).expect_err("unknown").contains("android or ios"));
        // An iOS foldable passes the matrix; whether `y` is one is the device type's to say.
        let err = call(&mut s, "create", &json!({"name": "x", "device_type": "y", "runtime": "z", "form_factor": "foldable"}));
        assert!(!err.expect_err("no such type").contains("not supported on iOS"));
    }

    /// The parameters a dispatcher's "missing" error names: "missing number parameter `x`",
    /// "xr: missing field `action`", "give `angle` (0-180) or `posture`". Empty for any other error.
    fn missing_names(error: &str) -> Vec<String> {
        let tail = if let Some(at) = error.find("missing ") {
            &error[at..]
        } else if error.starts_with("give `") {
            error
        } else {
            return Vec::new();
        };
        let mut ticks = tail.split('`').skip(1).step_by(2).map(str::to_owned);
        if tail.starts_with("give") { ticks.collect() } else { ticks.next().into_iter().collect() }
    }

    /// A value of the type the schema gives a parameter, to get past its "missing" error.
    fn dummy(schema: &Value) -> Value {
        if let Some(first) = schema["enum"].get(0) {
            return first.clone();
        }
        match schema["type"].as_str() {
            Some("number" | "integer") => json!(1),
            Some("boolean") => json!(true),
            Some("array") => json!(["a"]),
            Some("object") => json!({}),
            _ => json!("a"),
        }
    }

    /// What probing one method on one dispatcher found.
    #[derive(Debug, PartialEq)]
    enum Probe {
        /// The dispatcher checked this method's parameters: every parameter it reported missing is
        /// required or a member of a group of alternatives, a call with the schema's required
        /// fields (and one member of each group it asked for) wasn't refused for a missing one,
        /// and dropping any required field made it say that field was missing (or the call worked).
        Probed,
        /// The dispatcher refused before it read parameters (no such hardware, a platform or form
        /// factor it doesn't serve, a state): nothing is known.
        Unprobed(String),
    }

    /// Drives `method` on the Android dispatcher, on the running fake device `device`, from the
    /// schema's required fields.
    fn probe_android(s: &mut Session, method: &str, device: &str) -> Probe {
        let schema = crate::input_schema(method).expect("schema");
        let members: Vec<String> = crate::schema::one_of(method).into_iter().flatten().collect();
        let required: Vec<String> =
            schema["required"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        let mut params = json!({});
        if schema["properties"].get("device").is_some() {
            params["device"] = json!(device);
        }
        for name in required.iter().filter(|n| *n != "device") {
            params[name] = dummy(&schema["properties"][name]);
        }
        // Give the parameters the dispatcher asks for beyond the required ones: each must be a member of
        // a group of alternatives, or the schema leaves out something every call needs.
        let mut added: Vec<String> = Vec::new();
        let mut last = call(s, method, &params);
        for _ in 0..8 {
            let Err(error) = &last else { break };
            let names = missing_names(error);
            let fresh: Vec<&String> = names.iter().filter(|n| params.get(n.as_str()).is_none()).collect();
            let Some(&first) = fresh.first() else { break };
            for name in &names {
                assert!(
                    required.contains(name) || members.contains(name),
                    "`{method}` refuses a call with its required fields {required:?} for a missing `{name}` ({error}): the schema must require it or group it"
                );
            }
            params[first] = dummy(&schema["properties"][first]);
            added.push(first.clone());
            last = call(s, method, &params);
        }
        for group in crate::schema::one_of(method) {
            let given = added.iter().filter(|n| group.contains(n)).count();
            assert!(given <= 1, "`{method}` needed more than one of {group:?}: they are not alternatives");
        }
        assert!(added.iter().all(|n| members.contains(n)), "`{method}`: {added:?}");
        let refused = last
            .as_ref()
            .err()
            .is_some_and(|e| !missing_names(e).is_empty() && missing_names(e).iter().all(|n| params.get(n.as_str()).is_none()));
        assert!(!refused, "`{method}` still refuses {params} for a missing parameter: {last:?}");
        // Drop each required field in turn: the dispatcher must say it is missing, or (when the whole
        // call was refused before parameters) tell nothing.
        let mut confirmed = 0;
        let others: Vec<&String> = required.iter().filter(|n| *n != "device").collect();
        for name in &others {
            let mut without = params.clone();
            without.as_object_mut().map(|o| o.remove(name.as_str()));
            match call(s, method, &without) {
                Ok(_) => panic!("`{method}` works without `{name}`, but its schema requires it"),
                Err(error) if missing_names(&error).contains(name) => confirmed += 1,
                Err(_) => {}
            }
        }
        if last.is_ok() || (!others.is_empty() && confirmed == others.len()) {
            Probe::Probed
        } else {
            Probe::Unprobed(format!("{:?}", last.err()))
        }
    }

    /// Methods the Android dispatcher serves that this test must not run, and why.
    const NOT_RUN_ON_ANDROID: &[(&str, &str)] = &[
        ("boot", "starts or stops emulators: the fake's advertisement names this process"),
        ("boot_many", "starts emulators"),
        ("shutdown", "signals the advertised process, which is this one"),
        ("delete", "refuses a running device before it reads parameters"),
        ("create", "its required parameters depend on `platform`, and it creates a device"),
        ("clone", "creates a device from a golden that has booted"),
        ("ls", "without a path it lists the device's `/`, in no app's container; android_files.rs runs it on the files fake"),
        ("du", "without a path it sizes the device's `/`, in no app's container; android_files.rs runs it on the files fake"),
        ("kill", "the probe's pid is 1, init, which kill refuses without force (its own tests read its parameters)"),
    ];

    /// The parameters an iOS dispatcher arm reads with `need_*` or reports missing, found in its
    /// source: the iOS backend only builds on macOS, so on other hosts this is the nearest thing to
    /// running it. Arms that hand `p` to a helper (`tap_samples(p)`) read nothing here; the Android
    /// probe covers the helpers the platforms share.
    fn ios_arm_reads(source: &str, method: &str) -> Vec<String> {
        let quoted = format!("\"{method}\"");
        let starts_arm = |line: &str| line.starts_with("        \"") && line.contains("=>") && !line.starts_with("         ");
        let mut reads = Vec::new();
        let mut in_arm = false;
        for line in source.lines() {
            if starts_arm(line) {
                in_arm = line.split("=>").next().is_some_and(|names| names.split('|').any(|n| n.trim() == quoted));
            } else if line.starts_with("        _ =>") || line.starts_with("        other =>") || line == "    }" {
                in_arm = false;
            }
            if !in_arm {
                continue;
            }
            for marker in ["need_str(p, \"", "need_f64(p, \"", "need_bool(p, \"", "parameter `", "missing parameter `"] {
                let mut rest = line;
                while let Some(at) = rest.find(marker) {
                    rest = &rest[at + marker.len()..];
                    let end = rest.find(['"', '`']).unwrap_or(rest.len());
                    let name = rest[..end].to_owned();
                    if !name.is_empty() && !reads.contains(&name) {
                        reads.push(name);
                    }
                }
            }
        }
        reads
    }

    /// Every method's schema agrees with its dispatchers about what is required:
    ///
    /// - Android: each method its dispatcher serves is driven on a running fake device, in both
    ///   directions ([`Probe::Probed`]). A method the dispatcher refuses before reading parameters
    ///   isn't counted as probed; each such one is listed here with why, and a method that stops
    ///   needing to be listed fails until it is taken off.
    /// - iOS: its dispatcher only builds on macOS, and the Linux stub refuses every device method
    ///   before reading parameters, so the iOS arms' own `need_*` reads are scanned from the source:
    ///   each must be required or in a group of alternatives in the schema.
    #[test]
    fn the_schema_requires_what_the_dispatchers_require() {
        use sim_android::testing::fake_emulator;
        use sim_core::form_factor::FormFactor;
        let root = std::path::PathBuf::from("/tmp").join(format!("sa-schema-{}", std::process::id()));
        let sdk = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&sdk).expect("image dir");
        std::fs::write(sdk.join("system.img"), b"s").expect("system.img");
        std::fs::write(sdk.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        // A watch and a car: the methods that need their hardware are refused on a foldable before
        // their parameters are read.
        for (dir, tag, build_prop) in [
            ("android-34/android-wear/arm64-v8a", "android-wear", ""),
            ("android-34-ext9/android-automotive/arm64-v8a", "android-automotive", "ro.product.name=sdk_car_arm64\n"),
        ] {
            let d = root.join("sdk/system-images").join(dir);
            std::fs::create_dir_all(&d).expect("image dir");
            std::fs::write(d.join("system.img"), b"s").expect("system.img");
            std::fs::write(d.join("source.properties"), format!("AndroidVersion.ApiLevel=34\nSystemImage.TagId={tag}\n")).expect("props");
            if !build_prop.is_empty() {
                std::fs::write(d.join("build.prop"), build_prop).expect("build.prop");
            }
        }
        let env = emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") };
        let mut android = sim_android::Android::new(env);
        android.running_dir = Some(root.join("running"));
        android.create("fold", "system-images;android-36;google_apis;arm64-v8a", Some(FormFactor::Foldable)).expect("create");
        android.create("watch", "system-images;android-34;android-wear;arm64-v8a", None).expect("watch");
        android.create("car", "system-images;android-34-ext9;android-automotive;arm64-v8a", None).expect("car");
        // The app methods run adb: the fake one, so the probe never reaches a host's.
        sim_android::testing::fake_adb(&android);
        // One fake emulator is advertised at a time (its advertisement is named for this process).
        let mut fakes = vec![fake_emulator(&android, "fold")];
        let mut advertised = "fold";
        let backend = android.clone();
        let mut s = Session::with_android(android);

        let (mut probed, mut unprobed) = (Vec::new(), Vec::new());
        for (method, ..) in METHODS {
            if !platforms(method).contains(&Platform::Android) || NOT_RUN_ON_ANDROID.iter().any(|(m, _)| m == method) {
                continue;
            }
            let device = match *method {
                "rotary" => "watch",
                "vehicle" => "car",
                _ => "fold",
            };
            if device != advertised {
                fakes.push(fake_emulator(&backend, device));
                advertised = device;
            }
            match probe_android(&mut s, method, device) {
                Probe::Probed => probed.push(*method),
                Probe::Unprobed(why) => unprobed.push((*method, why)),
            }
        }
        assert!(
            unprobed.is_empty(),
            "the Android dispatcher refused these before reading their parameters, so nothing is known: {unprobed:?}"
        );
        // The probe reaches parameter checks (a method it didn't would be in `unprobed`).
        for method in [
            "tap",
            "swipe",
            "touch",
            "button",
            "type",
            "key",
            "location",
            "rotate",
            "rotary",
            "fold",
            "console",
            "touchpad",
            "stylus",
            "xr",
            "vehicle",
            "screen",
            "screenshot",
            "info",
            "devices",
        ] {
            assert!(probed.contains(&method), "{method} should be probed: {probed:?}");
        }
        for (method, _) in NOT_RUN_ON_ANDROID {
            assert!(METHODS.iter().any(|(m, ..)| m == method), "{method} is not a method");
        }

        // The iOS dispatcher's reads, from its source.
        let source = include_str!("ios.rs");
        let mut scanned = 0;
        for (method, ..) in METHODS {
            if !platforms(method).contains(&Platform::Ios) {
                continue;
            }
            let schema = crate::input_schema(method).expect("schema");
            let members: Vec<String> = crate::schema::one_of(method).into_iter().flatten().collect();
            let reads = ios_arm_reads(source, method);
            scanned += usize::from(!reads.is_empty());
            for name in reads.iter().filter(|n| *n != "device") {
                let required = schema["required"].as_array().is_some_and(|r| r.iter().any(|v| v == name.as_str()));
                // A parameter only iOS reads (`create`'s `device_type`) is optional in the schema and says so.
                let ios_only = schema["properties"][name]["description"].as_str().is_some_and(|d| d.contains("iOS only"));
                assert!(
                    required || members.contains(name) || ios_only,
                    "the iOS `{method}` arm reads `{name}` as required, but its schema doesn't ({reads:?})"
                );
            }
        }
        // The scan finds real reads: `launch`'s bundle id, `defaults_write`'s key.
        assert!(ios_arm_reads(source, "launch").contains(&"bundle_id".to_owned()));
        assert!(ios_arm_reads(source, "defaults_write").contains(&"domain".to_owned()));
        assert!(scanned >= 20, "{scanned} methods with reads");
        std::fs::remove_dir_all(root).ok();
    }

    /// The memory tools (#183) are iOS methods: listed for iOS, refused on an Android device with
    /// the platform they work on, and each takes an app by bundle id or pid.
    #[test]
    fn memory_tools_are_ios_methods_that_take_an_app() {
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        let (mut s, root) = android_session("mem");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "phone", "system_image": mobile})).expect("phone");
        for method in ["mem_leaks", "mem_heap", "mem_vmmap", "mem_history"] {
            let row = help.as_array().and_then(|a| a.iter().find(|m| m["method"] == method)).unwrap_or_else(|| panic!("{method}"));
            assert_eq!(row["platforms"], json!(["ios"]), "{method}");
            let err = call(&mut s, method, &json!({"device": "phone", "bundle_id": "com.example"})).expect_err(method);
            assert_eq!(err, format!("`{method}` isn't available on Android devices (it works on iOS)"));
            let schema = crate::input_schema(method).expect("schema");
            assert_eq!(crate::schema::one_of(method), [["bundle_id", "pid"]], "{method}");
            assert_eq!(schema["properties"]["pid"]["type"], "integer", "{method}");
            assert_eq!(schema["properties"]["raw"]["type"], "boolean", "{method}");
        }
        let history = crate::input_schema("mem_history").expect("schema");
        assert_eq!(history["required"], json!(["device", "address"]));
        let launch = crate::input_schema("launch").expect("schema");
        assert_eq!(launch["properties"]["malloc_stack_logging"]["type"], "boolean");
        std::fs::remove_dir_all(root).ok();
    }
}
