# The Android backend: emu

simee's Android devices run on `emu`, its own Android emulator (called emu-rs before the project
was renamed simee), imported from
[kaeawc/auto-mobile-emulator](https://github.com/kaeawc/auto-mobile-emulator) at `f0ccf83`. This
page is that project's README, kept for its status, provisioning, benchmark and performance notes;
paths are this repository's. How it fits the combined project: [architecture.md](architecture.md);
which Android form factors work: [form-factors.md](form-factors.md).

simee is an Android emulator written from scratch in Rust for Apple silicon. It boots the SDK's
unmodified arm64 system images on Hypervisor.framework and presents the same board, ports and
control surfaces as the official emulator, so existing tooling (`adb`, `adb emu`, test
harnesses such as [AutoMobile](https://github.com/kaeawc/auto-mobile)) can drive it unchanged.
The goals are to be able to run in the official emulator's place, to be measurably faster, and
to use fewer resources.

## Status

| Area | State |
| --- | --- |
| VM core | vCPUs, the framework's in-kernel GICv3, PSCI (SMP, power off and reset), arch timer, PL011 console, PL031 RTC |
| Board | Stock memory map and virtio-mmio slots, so the unmodified fstab and first-stage init find `vda`–`vde` and their by-name paths |
| Storage | virtio-blk with indirect descriptors, event-idx, and discard/write-zeroes (hole punching). System and vendor are copy-on-write in memory; userdata, cache and metadata persist per AVD. Userdata is created by a native ext4 formatter (no `mkfs.ext4`) |
| Boot | Full Android boot to `sys.boot_completed`: 9.5 s cold, 3.5 s warm (stock: 22.9 s and 17.1 s; [benchmarks](benchmarks.md)); first boot formats and encrypts `/data`. System images from API 29 on; emu refuses older ones up front with the reason (`EMU_ALLOW_OLD_IMAGES=1` boots them anyway, to work on them). API 29 and 30 draw with the GLES encoder over the goldfish `opengles` pipe: each pipe gets a render channel of its own in the renderer, as stock gives it ([performance notes](#performance-notes)); API 30 `google_apis` first-boots in 18.0 s (stock 17.0 s), and both idle at 2.1–2.9% of a host core (stock 3.7–5.4%; [benchmarks](benchmarks.md#api-29-and-30-first-boots-and-idle-host-cpu)); the board has no virtio-gpu for them (their kernels oops on it, and stock gives them none). They also get adb over `qemud:adb` (a client that connects first waits for adbd's `accept`), the reference RIL's `qemud:gsm` modem, and `logcat -Q`'s `logcat` stream from stock's `qemu.cmdline` and `qemu.logcat` properties. API 29 `default` warm-boots in 4.8 s (stock 6.4 s) and first-boots in 3.7 s (stock 4.8 s); its first boot runs the host GLES on SwiftShader, since ANGLE on Metal faults there ([performance notes](#performance-notes)); API 29 `default` and API 30 `google_apis` pass the AutoMobile harness (40 of 41 on API 30, 35 of 41 on API 29; device snapshots and video recording fail, and rotation and dark theme on API 29 fail on stock too; since the touch screen became stock's `virtio_input_multi_touch_1` (#69), AutoMobile's injected taps, multi-touch and pinch are cancelled on API 30, 2026-11-26). API 31 and 32 draw over the same pipe with no virtio-gpu (bootconfig `gltransport=pipe`, as stock gives every image): the gralloc mapper inside their `media.swcodec` opens the GLES transport to read color buffers, and their SELinux policy denies `mediaswcodec` the render node (`avc: denied { read write } ... name="renderD128" scontext=u:r:mediaswcodec:s0 tcontext=u:object_r:gpu_device:s0`) but not the goldfish pipe, so over virtio-gpu `screenrecord`'s encoder crashed ([#64](https://github.com/kaeawc/simee/issues/64)). API 33 guests set up address-space graphics with the protocol before the renderer's; the GPU translates it (`virtio/legacy_asg.rs`, [#39](https://github.com/kaeawc/simee/issues/39)). API 31 to 33's GLES is the encoder on a renderer with GL, as stock runs them, since their ANGLE finds SurfaceFlinger no EGL config. Images without `AndroidbootProps2` (API 30 and older, kernels without bootconfig) get stock's legacy kernel command line (`qemu.*` names) instead of bootconfig |
| adb | virtio-vsock with a host bridge on `console port + 1`, announced to the adb server. `adb shell`/`root`/`push` work, and the device shows up about 8 s after launch on a warm boot |
| Apps | The API's `install`, `uninstall`, `launch`, `terminate`, `apps` and `open_url` run on Android devices through the host's `adb` (the SDK's platform-tools, else the PATH), so they work on a stock emulator's AVD too, with iOS's parameters and result shapes. `install` is `adb install -r` for an `.apk` (an `.aab` through `bundletool` when it is on the PATH, else an error that says so); `uninstall` is `pm uninstall`; `launch` starts the package's own first launcher activity (`cmd package query-activities --brief --components`, so an app with two, as a debug build with LeakCanary's "Leaks" alias has, never gets the system's chooser), runs `am start -W` (`-S` when `restart` is true, the default, as iOS terminates first) and returns the app's `pid` from `pidof`, polled for up to 3 s (null when no process is named for the package), with the `activity` it started; `terminate` is `am force-stop`; `apps` lists third-party packages (`all` adds the system's) as bare package names, without labels, which need `aapt` or a `dumpsys` per package; `open_url` is `am start -a android.intent.action.VIEW -d`. Package names and URLs are validated and quoted before they reach the guest's shell. Every adb call has a deadline (an install or `pm uninstall` 300 s, `am start` 30 s, the rest 10 s); one past it is killed and reported by its command line, and each `.aab` install builds in a scratch directory of its own. `launch`'s `args`, `env` and `shaped` are refused on Android. Tested against a fake adb; not yet timed on a booted device |
| Console | `adb emu` / telnet console with token auth: `avd name`, `ping`, `kill`, `sensor set\|get\|status`, `event send\|text`, `geo fix`, `finger touch\|remove`, `power`, `rotate`, `sms send`, `gsm call\|accept\|busy\|cancel\|hold\|list\|voice\|data\|status\|signal`, `screenshot` turned upright as the guest rotated its display (as `adb screencap` is), `network speed|delay|status` (stock's presets, numbers and report, seeded by `-netspeed`/`-netdelay`; applied live in the NAT, `crates/emu-vmm/src/net/shape.rs`: a token bucket per direction, 0 = unlimited, each direction holds about a second of its rate and drops past that, as a bottleneck does (except small control frames, ARP replies and payload-free TCP segments, which take a bounded reserve of 16 frames past it), with TCP held back by the download queue's room; unshaped devices take the plain path with no shaper; inbound frames are delayed by a latency between min and max, in order. Verified in unit tests over loopback only: not yet measured in a guest under `network speed edge`, and `network speed` doesn't change the radio's reported data type), simee's `network loss <percent>|<up>:<down>|none` (stock has no loss command: each direction drops that percent of the guest's TCP, UDP and ICMP packets at the NAT, never ARP and never adb, which doesn't cross the NAT; drawn from a random stream per direction seeded once per boot; `network status` adds a download and an upload loss line while either is set; unit-tested with fixed seeds, not yet measured in a guest), `avd snapshot` (answers as stock does without snapshot support: `save`/`load` are a `KO`, `list` is empty, `del` succeeds), plus simee's `touch`, `clipboard`, `frames`, `sms list`, `xr` (Android XR), `car` (Automotive) |
| Discovery | `-list-avds`, `pid_<pid>.ini` running-instance file, `emulator-<port>` serial on stdout |
| Graphics | Home screen and apps render. virtio-gpu with gfxstream contexts on the host GPU: the guest's GLES through ANGLE on its Vulkan and, on images that map it (API 36 and newer, `VulkanVirtualQueue`), its UI and SurfaceFlinger drawn with Skia on that Vulkan, as stock's `GuestAngle` mode does, so the renderer runs without a GL backend (see "Vulkan-only renderer" below); Vulkan on MoltenVK (lavapipe fallback), Vulkan composition. Host-visible memory via blob mapping; goldfish address space (PCIe, shared-slot allocator) and goldfish pipe (refcounted color buffers) for the ranchu gralloc; guest `screencap` and host screenshots both work |
| Input & sensors | virtio-input touchscreen, keyboard and rotary, plus stock's dual-mode mouse on XR images. The keyboard is stock's `qwerty2` (0627:0001, stock's key set), so goldfish's `qwerty2.kl` applies: HOME, D-pad centre (`KEY_CENTER`), Recents (580), media and stem keys arrive. The console takes every stock `KEY_*` name and numeric codes. `qemud:sensors` lists the AVD's sensors by stock's rules (`hw.sensor*` flags; heart rate and wrist tilt on Wear OS API 28+; the hinge on foldables) with live values; GNSS over `qemud:gps`; goldfish battery; fingerprint; clipboard |
| Telephony | AT modem on a virtio-console `modem` port: the radio HAL registers on LTE with a loaded SIM; incoming SMS and calls from the console |
| Bluetooth & UWB | A built-in Bluetooth 4.2 controller on stock's `bluetooth` port and a UCI responder on its `uwb` port (a named console, the guest's `hvc2`), with no peers: the stack starts at boot and stays up, `cmd bluetooth_manager disable`/`enable` both work, UWB reports READY, and `IUwb` is looked up once instead of about once a second (API 36, 5 minutes of logcat, 2026-11-25). Discovery finds nothing; pairing needs netsim ([research](research/bluetooth.md), [#31](https://github.com/kaeawc/simee/issues/31)) |
| Audio | virtio-snd as ALSA card 0 (one playback and one capture stream; S16/S24/S32/float, 8–48 kHz, mono or stereo), on images that enable `VirtioSndCard` (phone, Wear, TV, XR) and on Automotive, like stock. The ranchu audio HAL plays through it: guest output reaches the host's default output device through CoreAudio, resampled to the device's own rate. A host stream is open only while the guest's PCM stream runs, so a device that plays nothing opens no host audio. Capture is silence unless `-allow-host-audio`, which records the host's default microphone, as stock; `-no-audio` keeps the card but opens no host device. Devices simee starts (`simee boot`, `boot_many`, the API) are silent (`-no-audio`) unless booted with `audio` (`simee boot --audio`); the desktop app boots with audio, since a person is watching. Booted with the card on phone API 35, 36, 37, Wear (API 37) and TV (API 36) |
| Network | virtio-net at stock's net slot (26, MAC `52:54:00:12:34:56`), so the guest has the `eth0` the radio HAL configures from the modem's `+CGCONTRDP` reply (10.0.2.15/24, gateway 10.0.2.2, DNS 10.0.2.3). Behind it, a user-mode NAT in the emu process (`crates/emu-vmm/src/net`, smoltcp for TCP; no root, entitlement or host interface): TCP, UDP and ping (unprivileged ICMP sockets) to anywhere the host reaches; 10.0.2.2 is the host's loopback and 10.0.2.3 forwards to the host's resolver (`/etc/resolv.conf`), as on stock. The cellular network validates (NetworkMonitor's HTTPS probe returns 204). The guest is untrusted, so its flows are capped by emu's open-file limit (raised at start to 10240 on macOS): 1024 TCP connections (256 still connecting; a SYN past that is reset), 512 UDP and 64 ICMP flows, with 128 descriptors kept for the console, adb bridge and display; a DNS flow closes at its last answer. `EMU_NET=link` keeps the link and drops the NAT (for `scripts/net-bench.sh`). Not yet: IPv6, inbound port forwarding (`redir`), IP fragments from the guest, VPN-scoped resolvers |
| Images | Phone API 35, 36, 37 and Wear boot (5–8 s warm; Wear gets stock's +512 MB below 1 GB). Phone API 31, 32 and 33 (`google_apis`) boot to the launcher and API 31 and 33 show Gboard (2026-11-26; boot times not yet measured); the Bluetooth stack on API 31 and 32 stays up since the controller answers Read Encryption Key Size (#78; an API 32 phone's adapter stayed on for 5 minutes with no stack restart and 0 crashes, 2026-11-26), and `screenrecord` records on both since they draw over the goldfish pipe (#64; 5 s while swiping: 159 frames at 720x1280 on API 31, 168 on API 32, 2026-11-26). AutoMobile's harness (booted `-no-audio`): API 31 and 33 pass 40 of 43 tools and API 32 39 (all fail the two audio tools and snapshots; API 32 also `recentApps`, over virtio-gpu too, 2026-11-26). Automotive: the vehicle HAL channel (`qemud:car`) and the virtio-snd card are implemented but not yet booted ([Automotive](#automotive)). Android XR (`google-xr`, `android-xr-*`, `ai-glasses`; provisioned from the `google_xr` site) gets the dual-mode mouse and `qemud:xr_service` (console `xr`), not yet booted: it gets the virtio-snd card too |
| Wear OS | New AVDs: 512 MB (booted with stock's +512 MB), `hw.mainKeys=yes`, no D-pad, and for round screens `boot.prop`'s `ro.emulator.circular=true`, which reaches the guest as a `qemu-props` property and bootconfig `androidboot.emulator.circular=1`. The rotary turns 15 units (degrees) per click, like stock; the console's `EV_REL:REL_WHEEL:<n>` stays raw |
| TV | New AVDs: 2 GB, `hw.mainKeys=yes`, `hw.dPad=yes`. The D-pad keys, centre included, reach the guest through `qwerty2.kl`; not yet checked on a running TV image |
| Hardware keys | `qemu.hw.mainkeys` follows the AVD's `hw.mainKeys` (stock's default yes hides the navigation bar); phone, tablet, foldable and automotive AVDs created by simee write `no` |
| AutoMobile | API 37: 38 of 44 harness tools pass; open: network shaping, gRPC audio, VM snapshots. The console now accepts `network` and answers `avd snapshot` the way stock does when it can't snapshot; the harness hasn't been re-run since |
| Desktop app | simee's window (`desktop/`), shared with iOS devices: AVD list, start/stop, live guest console, live screen with touch and form-factor controls (Back/Home/Recents/Power, rotate, hinge, rotary, D-pad) over `<avd>/simee/display.sock` |

## Quick start

```bash
cargo run -p simee-emu -- -list-avds
cargo run -p simee-emu -- -avd <name> -show-kernel -verbose   # first boot formats and encrypts /data
adb -s emulator-5554 shell                              # once adbd is up
adb -s emulator-5554 emu kill
make shim && export ANDROID_HOME=~/.simee/sdk          # make tools launch simee as "emulator"
cd desktop && cargo run                                 # the desktop window (iOS and Android)
simee list --platform android                          # the same AVDs through simee
```

`cargo run` signs binaries with the `com.apple.security.hypervisor` entitlement
(`scripts/sign-run.sh` is the cargo runner); Hypervisor.framework refuses unsigned callers.

Flags: `-avd`/`@name`, `-port`, `-cores`, `-memory`, `-read-only` (all disk writes kept in
memory, several instances per AVD), `-wipe-data`, `-show-kernel`, `-verbose` (prints vCPU exit
counts at shutdown), `-no-boot-anim`. The rest of the SDK emulator's flags are accepted and
ignored for now.

## Layout

| Path | What |
| --- | --- |
| `crates/emu-hvf` | Safe wrappers over Hypervisor.framework: VM, vCPUs, GIC, guest RAM with zero-copy file I/O. One of the few crates allowed `unsafe` (see AGENTS.md). |
| `crates/emu-vmm` | The machine: memory map (`layout.rs`), device tree (`board.rs`), vCPU loop and PSCI (`vcpu.rs`), MMIO bus, PL011/PL031, virtio-mmio transport, blk, rng, vsock, snd, host pipe services (adb bridge, `qemud:boot-properties`, `qemud:car`, ...) |
| `crates/emu-avd` | AVD discovery and config (`~/.android/avd`, `config.ini`, system image), form factor (`Avd::profile`), running-instance advertisements, bootconfig encoding |
| `crates/emu-sdk` | Java-free SDK provisioning: repository manifests, verified downloads, unpacking, the on-demand renderer (`emu -provision`, `-provision-renderer`) |
| `crates/emu-ext4` | Native ext4 formatter for first-boot userdata: writes only metadata, so a fresh 10 GiB image takes under 1 MB on disk |
| `crates/emu-fdt` | Flattened device tree writer |
| `crates/emu-container` | Host probe, backend selection (KVM, container, TCG) and the Redroid-style container launcher for Linux hosts with binder ([below](#container-backend-linux-with-binder-and-no-kvm)) |
| `apps/emu` | The `emu` CLI: SDK-emulator-compatible flags, per-AVD instance state, console server, display server |
| `crates/sim-android` | The client front ends use: AVDs as devices, starting `emu`, the console and display-socket clients |
| `desktop/` | simee's Rune window (both platforms), its own workspace |
| `docs/` | [Stock board reference](reference/stock-board.md), [desktop and display design](design/desktop-and-display.md), [roadmap](roadmap-android.md) |
| `scripts/` | Cargo runner that signs with the hypervisor entitlement, SDK overlay (`make shim`), AutoMobile harness (`make harness-android DEVICE=emulator-5554`) |

## Which `emu` simee starts, and AVDs the stock emulator runs

`simee boot`, `simee boot-many` and the desktop start the release `emu`, because a debug guest
boots several times slower. In order: `$SIMEE_EMU` (or `$EMU_BIN`) as given; an `emu` beside the
running executable unless that is a debug build's; this checkout's `target/release/emu`; an `emu`
beside the executable or `target/debug/emu` only when no release build exists; `emu` on the PATH.
A release `emu` older than the sources (`apps/emu`, `crates/emu-*`, `crates/sim-core`) is still
used, with a note on stderr; simee never builds it for you (`cargo build --release -p simee-emu`, then
`codesign -s - -f --entitlements scripts/hypervisor.entitlements target/release/emu`).

An AVD that `emu -create-avd` makes runs under the stock emulator too. Its `config.ini` has no
`fastboot.*` keys, so stock keeps its own default and quick-boots it from a snapshot as it would
an avdmanager AVD; emu has no snapshots and ignores those keys. (Earlier builds wrote
`fastboot.forceColdBoot = yes`, which made every stock boot of such an AVD cold: 15 s instead of
2.8 s. Delete that line from an older AVD's `config.ini` to get quick boots back.)

AVDs the stock emulator runs list as `Booted`. The stock emulator (36.x, macOS) does not write the
`pid_<pid>.ini` that simee's `emu` writes to `~/Library/Caches/TemporaryItems/avd/running`; it
holds `<avd>/multiinstance.lock` and writes the qemu PID to `<avd>/hardware-qemu.ini.lock`. simee
reads that PID, checks it is alive and a qemu started with the exact pair `-avd <name>`, and takes
the ports from `-port N` (adb is N + 1) or `-ports C,A`, else from the emulator-range port it
listens on. The process facts come from the kernel for that one PID, with nothing spawned:
`/proc/<pid>` on Linux, libproc on macOS (`sim-android`'s `host.rs`), where a host-wide `ps -axo`
and `lsof` cost ~60 ms each. A call on one AVD reads only that AVD's lock. A server remembers a
qemu's command line and ports by (PID, start time), so a PID the kernel hands to another program
is asked about again; an emulator is never listed from memory alone, as every call reads the locks
and checks the PID is alive. The parsed pointer and `config.ini` of each AVD are remembered by
(path, mtime, ctime, size, inode) in `emu-avd` (the change time catches a same-size file put back with its old mtime), except files changed in the last 2 s, so resolving a
device reads no AVD file that hasn't changed. The listing of the AVD home is remembered too, by the
directory's own stamp, which an entry that comes, goes or is renamed over changes; a listing
with a link among the pointers or a failed read is not remembered.
`simee info` shows `running.emulator` (`stock` or `simee`), the serial, ports and PID. What needs
simee's `emu` (touch, keys, wheel and frames over `display.sock`, the touchpad) says so and names
the fix instead of failing on a missing socket. `shutdown` interrupts the qemu process
(`kill -INT`, a clean exit for the stock emulator with no console session), after re-checking
that the PID still is a qemu running that AVD. `simee list` does not mark the emulator yet: `DeviceInfo` has no
field for it.

## Rendering modes

`emu -gpu MODE` takes the stock emulator's names, and an AVD's `hw.gpu.mode` in `config.ini`
applies when no `-gpu` flag is given (the flag wins). An unknown mode is an error that lists
the valid ones. Every mode renders through gfxstream; what differs is the Vulkan driver under it.

| Mode | Runs on | Use it when |
|---|---|---|
| `auto`, `host` (default) | the host GPU through MoltenVK/KosmicKrisp; lavapipe if MoltenVK fails here | a normal desktop session |
| `lavapipe`, `swangle`, `angle_indirect` | lavapipe, software Vulkan on the CPU | no GPU, CI, headless hosts |
| `swiftshader`, `swiftshader_indirect` | SwiftShader, software Vulkan on the CPU (Linux hosts only; see below) | comparing against lavapipe |
| `software`, `guest` | lavapipe, with stock's notice that guest rendering isn't supported | an AVD configured that way keeps booting |
| `off`, `none` | no renderer; the guest boots without graphics | boot-only checks (flag only; `hw.gpu.mode=off` in `config.ini` is ignored) |
| `metal` (flag only, experimental) | `host`, and posted frames also go to simee's encoder as IOSurfaces copied on the GPU instead of read back | measuring the Metal present path (#356, docs/research/metal-present.md) |

SwiftShader doesn't work on macOS hosts: the renderer aborts at init with "Failed to find memory
type for ColorBuffers" (6 of 6 boots on API 30 and 36, M3 Max, 2026-11-27, #397). So `emu -gpu
swiftshader` (or `swiftshader_indirect`) is refused up front with "use -gpu lavapipe", and an AVD
whose `hw.gpu.mode` says `swiftshader` runs lavapipe instead and prints a notice. Linux hosts are
unchanged.

Rendering inside the guest (virtio-gpu 2D with Mesa in the image) isn't possible with SDK
images: a scan of the API 34 to 37 system images found none that ships a guest renderer, and
stock 37.2.13 reports "the system image does not support guest rendering" and falls back to
lavapipe for an AVD configured for it. `emu` does the same and prints that notice once.

## Benchmarks

`scripts/android-bench.sh` boots an AVD the same way under either emulator and measures it. The
current numbers, cold and warm, and how they were taken are in [benchmarks.md](benchmarks.md).

Both run guest code natively on the CPU. The differences are in the VMM: no
userspace GIC, fewer device threads, and RAM that is only committed when touched.

## Provisioning without Java

simee needs a system image, platform-tools (adb) and, for graphics, the gfxstream renderer.
Java, sdkmanager, avdmanager, Python, e2fsprogs and the full emulator package aren't required
(userdata is formatted natively by `crates/emu-ext4`). The `emu` binary provisions the rest
itself:

```bash
emu -list-images -tag google_apis                       # stable arm64 images for this host
emu -provision 36 -create-avd small36                   # platform-tools + google_apis image + renderer + AVD
emu -provision 35 -tag google_apis_playstore -create-avd play35 -lcd 480x800@240 -cores 2 -memory 1536
emu -provision 36 -dry-run                              # resolve only: URL, size, sha1, install dir
emu -provision 36 -no-renderer                          # skip the renderer (boots without graphics)
emu -provision 34 -tag android-automotive               # no plain 34 image: picks the newest, 34-ext9
emu -provision-renderer                                 # install or update just the renderer
emu -create-avd NAME -sysdir system-images/android-36/google_apis/arm64-v8a -lcd 720x1280@320
emu -create-avd NAME -sysdir ... -no-golden             # a pristine first boot: no golden user data
emu -create-avd NAME -sysdir ... -profile software-lite # a phone for software mode: 540x960, 30 Hz (below)
```

`-create-avd` (and `-provision ... -create-avd`) also keeps one settled "golden" user data per
system image: the first AVD of an image boots once to the launcher and is stopped, and every later
AVD of that image starts from a copy-on-write clone of it, skipping the first boot's extra minutes
(apexd decompressing the image's compressed APEX files, `system_server`'s first-boot work). How it
is keyed, locked and invalidated, and what it saves, is in [farm-android.md](farm-android.md#golden-user-data-per-system-image).

`crates/emu-sdk` reads Google's repository manifests (`repository2-3.xml`, and
`sys-img/<tag>/sys-img2-3.xml` per tag), picks the newest stable archive for the host
(platform-tools for the machine it runs on; system images are host-independent; the renderer is
Apple silicon's on macOS and the host's own elsewhere, so Linux x86_64 gets the `.so` one; Google
publishes none for arm64 Linux), downloads it with the system's `/usr/bin/curl` (resumable, into `$ANDROID_HOME/.downloads`),
checks size and sha1, unpacks it with `/usr/bin/unzip` and moves it into the standard SDK layout.
Installed packages are skipped, manifests are cached for a day, and the archive is deleted once
unpacked. Disk images (`*.img`) don't go through a plain unzip: each streams out of the archive
(`unzip -p`) into a file where only non-zero 4 KiB blocks are written, so zero runs stay holes.
Most of a system image is zeros: TV 34's 8.6 GB `system.img` takes 1.4 GB and reads back
byte-identical. Unpacking TV 34 took 64 s instead of 4 min 48 s (unzip writing 8.6 GB, then a pass
reading it back to punch holes) on a Linux VM, against a 40 s floor for inflating it at all. The SDK is `$ANDROID_SDK_ROOT`/`$ANDROID_HOME`, else Android Studio's default
(`~/Library/Android/sdk` on macOS, `~/Android/Sdk` on Linux). `SDK_TEST_BASE_URL` (the variable sdkmanager honours) points provisioning at another repository,
a mirror or a `file://` copy. An API level a tag publishes only
at extension levels (Automotive's API 34 and 35) resolves to the newest of them. Provisioning
works on Linux too, for preparing images and AVDs; there `-provision` skips the renderer, since
only Apple silicon boots guests so far, and `-provision-renderer` installs the Linux one (267 MB
on disk from a 350 MB archive, 37.3.10).

**The renderer** comes out of the SDK's `emulator` package, fetched on demand: the archive
(420.6 MB for 37.3.10) is verified like any other, but only `emulator/lib64/**` minus
`lib64/qt/**` (the stock window's Qt libraries) is unpacked, plus `LICENSE` and `NOTICE.txt`:
498 MB on disk instead of the package's 1.32 GB. That keeps everything the renderer loads: the
backend and its `@rpath` libraries, the shader translator it opens by name, `gles_angle`, and
the Vulkan loader, ICDs (MoltenVK, lavapipe with its LLVM, SwiftShader) and validation layers.
It goes to `$ANDROID_HOME/simee/renderer/<revision>/` (with a `renderer.properties` marker
recording revision and sha1, and a `current` symlink to the newest), never `emulator/`, so
sdkmanager's own package tracking is never confused. `-provision` installs it unless the SDK
already has a renderer; `-provision-renderer` updates to the newest stable revision and removes
older ones.

At boot emu uses the SDK's own `emulator/lib64` when present, else the newest
`simee/renderer/*/lib64`, and points gfxstream's launcher directory at its parent (where it
finds `lib64/vulkan` and `lib64/gles_*`). `emu-gfxstream` preloads the backend's dependencies
leaves-first from that directory (`@rpath` load commands on macOS, `DT_NEEDED` on Linux, whose
backend has no `RUNPATH`), so they resolve without any rpath. On macOS libraries gfxstream
`dlopen`s by bare name resolve through the binary's rpaths (`@executable_path/lib64`, which emu
links to the chosen `lib64`, then the build machine's `emulator/lib64` and
`simee/renderer/current/lib64`); on Linux they're preloaded too, since glibc matches a bare name
against what is already loaded: the shader translator, and ANGLE's `libEGL.so`/`libGLESv2.so`
from `gles_angle`, which the backend opens during init even without a GL backend (it crashes when
it finds no EGL). The Linux backend's symbol table is stripped, so the direct Vulkan readback
macOS uses isn't there; without a GL backend the exported `FrameBuffer::readColorBuffer` reads
the Vulkan image instead (byte-exact, 1.3 ms for 720×1280 on lavapipe; with GL it returned
wrong pixels and took 7 ms), so Linux runs the Vulkan-only renderer too. To check an install
without a VM: `cargo run -p simee-emu-gfxstream --example renderer_caps -- [-vulkan-only] <lib64>
[moltenvk|lavapipe|swiftshader]` prints capsets 3, 8, 9 and checks a 720×1280 readback.

AVD defaults favour cheap, fast boots: 720×1280 screen, 2 GB RAM, 4 vCPU, 4 GB data.
Benchmarks of the stock emulator found that smaller screens boot markedly faster and that RAM
above 2 GB slows cold boots down.

## Software mode (Linux without KVM)

In progress ([#55](https://github.com/kaeawc/simee/issues/55)). Cloud and CI workers often have no
`/dev/kvm` and no GPU, so every guest instruction has to be translated in software and every
frame drawn on the CPU. On an x86_64 Linux host, `emu -avd NAME` boots an AVD made from an
x86_64 image this way. The console port, adb, `display.sock` (screenshots and input) and `emu
kill` work as on a Mac. What follows records what was decided and why. The numbers are in
[benchmarks.md](benchmarks.md#android-software-mode-linux-without-kvm).

```bash
emu -provision 36 -tag default -abi x86_64 -create-avd phone
emu -provision-renderer
emu -accel-check    # which QEMU would run the guest and on what (KVM or TCG), or what's missing
EMU_QEMU=/path/to/qemu-system-x86_64 emu -avd phone   # QEMU 11.1 or newer; without it, PATH's
emu -avd phone -accel off   # TCG even where /dev/kvm is usable (-accel on: KVM or fail)
```

- **KVM when the host has it.** emu probes KVM the way QEMU will use it: `/dev/kvm` opens with
  what QEMU needs (`emu_kvm::open`, whose error says how to fix a missing or denied device), it
  creates a VM (which fails with EBUSY on kernels before 6.12 while VirtualBox or VMware holds
  VT-x), and the QEMU's `-accel help` lists `kvm` (a QEMU built with `--disable-kvm` doesn't).
  Then QEMU runs the guest with `-accel kvm -cpu host` and keeps everything else: the board,
  emu's devices over vhost-user and vfio-user, the shared memfd RAM. Otherwise the guest runs on
  TCG, and `-verbose` and `-accel-check` say why. `-accel` and `-no-accel` mean what they mean to
  stock: `auto` by default, `on` is KVM or fail, `off` is TCG.
- **Retry on TCG.** In `auto`, a QEMU that still fails on KVM is started once more on TCG, with
  a one-line notice, on the devices already served (their accept loops take the new
  connection). It retries only a failure that is KVM's: QEMU's stderr named KVM or the
  accelerator, or QEMU died silently before the guest wrote to its console; and only within
  10 s of starting, as a backstop (`soft::retry_on_tcg`). Any other failure is reported as is.
- **CI on GitHub's KVM.** GitHub's x86_64 hosted runners expose `/dev/kvm` as root:kvm 0660;
  `.github/actions/enable-kvm` opens it to the runner user. The `Software mode` workflow boots
  once on KVM and once with `-accel off`. The KVM leg then runs `scripts/bench/android-boot.sh`
  against the stock emulator, a pinned release checked against its SHA-256 in an SDK of its own,
  on the same runner: one round on a pull request, three on main or a manual run
  ([benchmarks](benchmarks.md#android-software-mode-on-kvm-githubs-x86_64-runner)). GitHub's
  arm64 hosted runners have no `/dev/kvm` (probed in #56), so the arm64 KVM backend can't run
  natively there.

- **Host:** x86_64 Linux with QEMU 11.1 or newer and the SDK's Linux renderer
  (`emu -provision-renderer`); no GPU. The smallest host it has run on is 4 cores and 3.8 GiB (a
  3.3 GiB cgroup limit), with a 1.5 GB guest (emu and QEMU at 2.3 GB PSS); the research host had 8 GiB.
  QEMU's peak RSS is about 3 GB for a 2 GB guest, and emu's about 0.25 GB (measured under TCG).
- **What runs where:** `emu_vmm::soft` builds QEMU's command line for stock's x86 board, serves
  every device but the disks, and supervises QEMU. QEMU resets the machine itself when the guest
  reboots. `apps/emu/src/software.rs` is the boot. AVDs from arm64 images are refused with the
  reason, since software mode has only the x86 board.

- **CPU: QEMU's TCG, 11.1 or newer, one translator thread per vCPU.** It is the fastest
  translator available and is multi-threaded; a translator library (Unicorn) runs one thread
  for all vCPUs, which took twice as long to reach zygote. QEMU 6.2 (Ubuntu 22.04's)
  panics the guest under multi-threaded TCG within seconds (`int3` in the kernel's
  live-patching window); 11.1 has QEMU's fix, but one full boot in about a dozen still hit
  it.
- **AutoMobile drives it.** `scripts/automobile-harness-android.sh emulator-5554` on an `emu -avd`
  device (2026-11-26, AutoMobile 0.0.96 from source): 41 of 44 tools pass, at a median of 7.9 s
  per tool. Every interaction and device-state tool passes (observe, taps, swipes, pinch,
  rotation, apps, sensors, SMS, calls, biometrics, network conditions, the system tray, video
  recording). The 3 that fail fail on a Mac too: `captureAudio` and `injectAudioInput` (AutoMobile
  reaches audio over the stock emulator's gRPC endpoint, which emu doesn't serve) and
  `deviceSnapshot` (emu has no snapshots). The harness records a swipe between `videoRecording`
  start and stop. Stopped right after starting, under TCG the guest's software encoder hadn't
  produced its first frame, so the muxer never started (`MediaMuxer: stop() is called in invalid
  state 1`) and the file stayed empty, in 2 of 2 full runs.
- **Idle cost.** Settled, a software-mode device takes about 0.7 of a host core. A quiet
  Perfetto trace of a clean guest (2026-11-27) puts about a third of that on vsync: the ranchu
  composer marks its present fences unreliable, so SurfaceFlinger keeps hardware vsync on and
  handles 60 composer callbacks a second while nothing draws (SurfaceFlinger and the composer: 2.4
  s of 8.2 s busy). Next come system_server (1.9 s) and the sensors HAL (0.9 s): system_server's
  rotation listener keeps the accelerometer at 15 Hz. An AutoMobile session adds its own sensor
  listeners and log readers on top.
  `hw.lcd.vsync=30` in the AVD's `config.ini` (emu honours it as stock does) settles 0.12 cores
  lower, with no frames lost at software mode's rate
  ([benchmarks](benchmarks.md#software-mode-through-emu--avd)).
- **The software-lite profile** (`emu -create-avd NAME -sysdir system-images/android-36/default/x86_64
  -profile software-lite`, also with `-provision … -create-avd`): a 540x960 screen at 240 dpi and
  `hw.lcd.vsync=30` together, for hosts where the guest and SwiftShader share a few cores. Measured
  against the default 720x1280 at 60 Hz on one 4-vCPU host (2026-11-28, median of 3 warm boots
  each, [benchmarks](benchmarks.md#software-mode-a-smaller-screen-and-30-hz)):

  | | default | software-lite |
  | --- | --- | --- |
  | Idle host CPU | 0.59 cores | 0.53 (30 Hz alone: 0.49) |
  | Host CPU while swiping | 2.43 cores | 2.01 (the screen alone: 2.15) |
  | Launcher frames in 20 swipes, warm / cold | 185 / 121 | 185 / 118 (the same) |
  | p90 frame time | 300 ms | 200 ms |
  | Warm boot to `sys.boot_completed` | 169.8 s | 181.9 s (spreads overlap) |
  | Memory (PSS) | 2868 MB | 2828 MB (no real change) |

  The two halves do not add: 30 Hz is what lowers the idle cost and the smaller screen is what
  lowers the swiping cost and frame times, and the profile gets about a tenth less idle CPU and
  about 17% less swiping CPU, with no more frames and no faster boot or less memory. It is
  opt-in because apps see the smaller, slower display; a default AVD stays 720x1280 at 60 Hz. It
  applies to phone images only (the other form factors have screens of their own and are refused),
  RAM and vCPUs stay as `-memory` and `-cores` say, and an explicit `-lcd` wins over its screen. An image's
  golden user data is keyed by screen size, so the first software-lite AVD of an image boots once
  to settle its own golden. `scripts/bench/software-profile.sh --arms "default lcd540 vsync30 lite shipped"` reproduces the table (the software-lite column is its `shipped` arm).
- **KFENCE sampling off** (`kfence.sample_interval=0` on the x86 board's command line). These
  kernels build KFENCE with static keys, so every 500 ms its sampler flips a branch inside every
  slab allocation, live-patching kernel text through an `int3`. Under multi-threaded TCG another
  vCPU can run the breakpoint after the patch finished: that was the `int3` panic in
  `__kmem_cache_alloc_node`, three CPUs at once. Turning it off cost nothing measurable
  ([benchmarks](benchmarks.md#software-mode-through-emu--avd)).
- **Guest: the x86_64 images on x86_64 hosts**, with stock's x86 board (virtio-pci: `system.img`
  at `00:03.0`, so `androidboot.boot_devices=pci0000:00/0000:00:03.0`, and the fstab's metadata at
  `00:06.0`). They run guest code 1.8 to 2.0 times as fast as arm64 images, which take 13% longer to
  reach zygote. arm64 images under QEMU's `virt` board work too: its memory map and virtio-mmio
  slot order are stock's arm64 board's. There the debug console is `hvc0`, and `goldfish-logcat`
  expects `hvc1`.
- **Guest CPU model `max,-erms,-fsrm,-fzrm,-fsrs,-fsrc`** (x86_64): without the fast-string
  features the guest kernel copies memory with plain moves, which TCG translates, instead of
  `rep movsb`, which it runs as a byte loop. Keep QEMU's 1 GiB translation cache.
- **Devices: emu, out of process.** Upstream QEMU can't run these images to `sys.boot_completed`
  on its own: without a gfxstream GPU, SurfaceFlinger aborts (`no suitable EGLConfig found`) and
  zygote restarts every 5 s, and the sensors HAL has no `qemud:sensors`. emu keeps the devices,
  console, adb bridge, display socket and frame ring, and serves them to QEMU over vhost-user
  (the virtio devices; QEMU 11's `SHMEM_MAP` lets a backend map GPU blobs into the guest) and
  vfio-user (goldfish address space, a plain PCI device). The vhost-user backend is
  `crates/emu-vmm/src/vhost_user.rs`: QEMU's guest RAM is a shared memfd
  (`-object memory-backend-memfd,share=on`) that emu maps once (`GuestRam::map_shared`), rings are
  translated through the memory table, kicks and calls are eventfds, and a raise calls only the
  queues whose used ring moved. vsock runs on it: the guest's adbd and qemud pipes reach emu's
  adb bridge and pipe services (`examples/vhost_user_vsock.rs`), with bulk transfers faster than
  QEMU's own user network and round trips level with it
  ([benchmarks](benchmarks.md#android-software-mode-linux-without-kvm)). The console, net and GPU
  go through QEMU's generic `vhost-user-test-device-pci` (virtio id, queue count, config size),
  so QEMU needs no code per device (`examples/soft_devices.rs` serves the set and prints the QEMU
  arguments).
- **GPU: emu's virtio-gpu over vhost-user.** The renderer runs Vulkan-only with exportable blobs
  (`USE_EXTERNAL_BLOB`); a host-visible blob the guest maps is exported once as an fd (gfxstream
  hands its own descriptor over, so emu keeps it for later maps) and QEMU maps it into the GPU's
  shared-memory BAR on `SHMEM_MAP` (the device's `ShmWindow`; on Apple silicon the same device
  maps host pages into the VM). With external blobs the renderer refuses
  `stream_renderer_resource_map`, so a mapping's size is the blob's from `RESOURCE_CREATE_BLOB`.
  Mapped blobs then show up in every vhost-user device's memory table as further fds: guest RAM is
  the fd that maps guest-physical 0. SurfaceFlinger's RenderEngine initializes Vulkan on it (132 ms
  in the guest) and frames reach emu.
- **Goldfish address space over vfio-user** (`crates/emu-vmm/src/vfio_user.rs`): the ranchu
  mapper aborts without it ("GoldfishAddressSpaceHostMemoryAllocator failed to open"). emu serves
  its config space and registers by message, and its 16 GiB area as a sparse memfd QEMU maps into
  the guest; the device model is emu's own, built once the guest has placed BAR1.
- **Goldfish pipe over vfio-user, described by an ACPI table.** `libqemupipe.ranchu.so` opens
  `opengles`, `GLProcessPipe` and `refcount` on `/dev/goldfish_pipe_dprctd` and falls back to
  vsock only when that open fails with `EAGAIN` (read from its disassembly; its other pipes go to
  vsock port 5000 first). Without the device the allocator can't open `refcount`, the composer
  gets no swapchain and SurfaceFlinger aborts ("Can't create AidlComposerClient"). On x86 the
  pipe driver binds only by ACPI (`GFSH0003`) or DT, and QEMU has no such device, so emu carries
  the pipe's registers in BAR0 of a vfio-user PCI function (607d:f154, INTA) at slot 0x1e, and an
  SSDT it generates (`crates/emu-vmm/src/acpi.rs`, QEMU's `-acpitable`) declares `GFSH0003` with a
  `_CRS` that reads that BAR through config I/O (0xCF8/0xCFC) and its IRQ from PIIX3's PIRQ
  route (`pc` routes slot S's INTA to PIRQ `(S - 1) & 3`). The function then hides from the OS
  (vendor `0xffff` once the firmware has placed BAR0 and turned decoding on): otherwise Linux
  finds the BAR overlapping the ACPI device's range, fails to claim it and moves it, away from
  where the pipe driver looks. INTx follows vfio's automasked model: raise, trigger and mask;
  QEMU unmasks when the guest next touches the device, and a line still up triggers again.
  Both goldfish functions need `x-no-posted-writes=true` (`vfio_user::qemu_device`): QEMU posts
  BAR writes by default, so the vCPU runs on before emu has seen a write. The pipe's guest reads a
  command's status from memory right after writing `CMD`, and frees the command page on close;
  posted, emu answered into a page that was by then something else, and guests panicked in
  `zap_huge_pmd` on a PMD of `0xffffffff` (a status of -1).
- **vhost-user memory slots.** With GPU blobs and the goldfish address space mapped, the guest's
  memory outgrows `SET_MEM_TABLE`'s 8 regions (QEMU: "Failed preparing vhost-user memory table
  msg", and every device's memory update fails), so the backend takes regions one at a time
  (`CONFIGURE_MEM_SLOTS`: `ADD_MEM_REG`/`REM_MEM_REG`, 256 slots).
- **Boots to `sys.boot_completed`.** With all of the above, the API 36 image boots under QEMU
  TCG on emu's devices with no KVM and no GPU
  ([benchmarks](benchmarks.md#boot-to-sysboot_completed-with-emus-devices)): 168 s warm, 0 of 5
  boots failed. The SDK emulator's own software mode (`-accel off`, refused without
  `ANDROID_I_WANT_MY_TCG=yes`) runs all vCPUs on one translator thread and took 2302 s (median
  of 3, one of which didn't finish in 40 minutes) on the same host. Open: the launcher's workspace composes black (the navigation bar and
  FallbackHome draw); the launcher once failed to make a bitmap at unlock (`DotRenderer`, null
  `Bitmap`); and under emulation apps can overrun Android's startup timeouts and show "isn't
  responding" dialogs.
- **Rendering: gfxstream on SwiftShader**, both from the SDK's Linux `emulator` package
  (`lib64` carries `libgfxstream_backend.so`, its lavapipe and SwiftShader; 238 MB without the Qt
  UI). gfxstream picks the driver itself (`ANDROID_EMU_VK_ICD`; it refuses the host's own
  lavapipe). The SDK's lavapipe (Mesa 26.2-devel, LLVM 21) crashes in a render thread minutes
  into a boot (a SIGSEGV inside `libvulkan_lvp.so` under `RenderThread::main`, every lavapipe
  boot); SwiftShader ran without one and booted faster.

When the host has KVM, software mode runs the guest on it (above); TCG is the fallback.

## Container backend (Linux with binder and no KVM)

In progress ([#210](https://github.com/kaeawc/simee/issues/210)): Android's userspace in a
container on the host's kernel, as Redroid does, for hosts that have binder and no usable KVM. The
research, environment matrix and the cited Redroid numbers are in
[research/container-backend.md](research/container-backend.md). What `crates/emu-container` has
so far, and what it doesn't:

- **Selection.** Owner decision 2026-11-27: with no `-backend`, KVM first; the container only on
  a host without usable KVM that has what it needs; then TCG software mode. `-backend
  kvm|container|tcg` forces one, and a forced backend that can't run is an error naming why, never a
  fallback. The container is considered only for a guest of the host's own architecture.
- **Host probe.** Required: Linux, binder (binderfs in `/proc/filesystems` that, test-mounted in a
  throwaway user namespace, holds `binder`, `hwbinder` and `vndbinder`: a plain `modprobe
  binder_linux` gives an empty one), memfd, the unified cgroup hierarchy, user namespaces (also for
  root: the container's process runs unprivileged), and, for an unprivileged user, `newuidmap` and
  `newgidmap` with a 65,536-id range in `/etc/subuid` and `/etc/subgid`; overlayfs on Linux 5.11
  or newer (`userxattr`). Reported but not blocking: cgroup memory/cpu/pids controllers, DMA-BUF
  heaps. Each missing piece prints its fix.
- **Image.** One pinned image: `redroid/redroid:15.0.0_64only-250627`, by per-architecture
  manifest digest and layer digest (`image.rs`). It is downloaded only when asked (`skopeo copy` of
  the manifest by digest) and every blob is hashed against the pin before it is unpacked.
- **Launcher.** `unshare` into new user, mount, pid, uts, ipc and cgroup namespaces (not network).
  Container root is never host root: an unprivileged user is their own container root with their
  `/etc/subuid` range above it (`newuidmap`); run as host root, the container's process drops to a
  range of its own (root's `/etc/subuid` range, else 2,000,000,000 + 65,536) before it creates the
  namespace, and the state directory is handed to that range. The script mounts an overlay of the
  unpacked image over a per-device writable layer, a `/dev` tmpfs with the nodes Redroid's init
  expects, binderfs (checked for the three devices, linked at `/dev/*`), `/proc` with runc's masked
  and read-only paths (`kcore`, `/proc/sys`, `sysrq-trigger`...), and the host's `/sys` with every
  mount in it read-only. A helper then `pivot_root`s into the overlay and detaches the old root
  (no `chroot`: root inside one can walk out), and execs `/init` with Redroid's arguments
  (`androidboot.redroid_width|height|dpi|gpu_mode`, `androidboot.use_memfd=true`). The helper is
  `emu` itself under a hidden subcommand (`helper::main`) or the crate's `simee-container-helper`
  binary, and it also sets `PR_SET_PDEATHSIG` so a crashed `emu` takes the container down with it
  (start containers from a thread that outlives them: the kernel ties the signal to the thread);
  dropping a `Container` kills it. Removing a rootfs the container made runs `rm` inside the
  namespace.
- **Verified:** the plan, the script, the probe and the selection against fake hosts; and on a Linux
  VM without binder, as root and as an unprivileged user, a real user namespace with the id maps,
  the overlay, `/dev`, devpts, `/proc` and its masks, `/sys` read-only throughout, and the pivot:
  the container's mount table has the overlay as its only root, no host device, no path to the
  host's files, and uid 0 maps to a non-zero host id; setup stops at binderfs with its message
  there. Also that a helper's child dies when its parent is killed, and that a tree owned by
  subordinate ids is removed from inside the namespace.
- **Not verified:** a boot. No binder host has run this launcher, so whether Redroid's `/init`
  starts unprivileged in this namespace set, and how long it takes, are open. It is not wired into
  `emu` yet, so nothing boots through it; there is no display, input or console for it, no
  network namespace (adbd listens on the host's port 5555, so one container per host), no cgroup
  limits, and no `docs/benchmarks.md` row (nothing measured). `SIMEE_E2E_CONTAINER=1 cargo
  nextest run -p simee-emu-container` runs the namespace tests against the host's kernel;
  `cargo run -p simee-emu-container --example probe` prints what a host offers.

## Automotive

What the AAOS sources show (`device/generic/car/emulator`, `hardware/interfaces/automotive/vehicle`,
the emulator's `qemu-setup.cpp`), what simee does with it, and what still needs a real boot.

**Vehicle HAL.** The emulator VHAL (`vhal_aidl/VehicleEmulator`, API 33–36) opens the qemud
`car` pipe only when `ro.boot.qemu` is set; if the open fails it logs and carries on, and its
property values come from the image's own JSON config. So the pipe doesn't gate boot: it is how
the host drives the car. Stock registers the service only for car images (`build.prop`'s
`ro.product.name`, `ro.product.system.name` or `ro.build.flavor` contains `car`); simee does
the same (`Avd::is_car`, falling back to an `android-automotive*` tag when an image has no
`build.prop`) and refuses the pipe on every other image, as before.

The channel is `libqemud` framing (4 hex digits of length; the guest sends 64 KiB and larger
payloads with a 4-byte big-endian `len | 0x8000_0000` header) around proto2
`vhal_proto.EmulatorMessage`. The guest reads into a 2048-byte buffer and ends its read loop on
a payload that fills it, so host frames carry at most 2047 bytes. It answers every command in
order and sends `SET_PROPERTY_ASYNC` on its own from boot, so simee drains the pipe whether or
not anyone is waiting. `sim_core::vhal` is the protocol (a hand-written codec, no protobuf
dependency, checked against hand-encoded stock commands), `emu_vmm::pipe::car` the pipe.

Stock has no console command for the car (its controls live in the UI and in gRPC's
`sendCarEvent`), so simee adds one, as an extension:

```
car set <property> [<area>] <value>    # SET_PROPERTY_CMD, as stock's car page sends
car get <property> [<area>]            # GET_PROPERTY_CMD
car list                               # GET_PROPERTY_ALL_CMD
car names                              # the property names the console knows
```

Properties are VHAL names (`GEAR_SELECTION`, `PERF_VEHICLE_SPEED`, `IGNITION_STATE`, ...; short
`gear`, `speed`, `ignition`, `night`, `parking-brake`) or numeric ids; values are typed by the
id's type bits (`drive`/`P`, `on`/`acc`, `12.5`, `true`, `2,0` for vectors, hex for bytes).
Value lines read `property=<name> id=<hex> area=<n> status=<s> value=<text>`. simee reaches it
as the `vehicle` API method and CLI command (`simee vehicle <avd> gear drive`), for devices
whose form factor is auto.

**Audio.** From API 35 the car audio HAL (`audio.primary.caremu`) calls `mixer_open(0)` and
then `mixer_get_name()` without a NULL check, so without ALSA card 0 it most likely crash-loops.
simee's virtio-snd card (one output and one input stream, S16/S24/S32/float, 8–48 kHz, mono or
stereo, no jacks; virtio-mmio slot 18, where stock uses PCI) is there for car images and for
every image that enables `VirtioSndCard`, the phone, Wear, TV and XR images included, as on
stock. `EMU_VIRTIO_SND=1` (or `0`) forces the card on (or off) for any image.

How it plays (`crates/emu-vmm/src/virtio/snd.rs`, `crates/emu-vmm/src/audio/`): buffers
complete at the stream's nominal rate on the host's clock, so the HAL's blocking writes keep
their timing whatever the host device does. Playback is decoded into a lock-free ring as the
guest queues it, and the host's CoreAudio callback (`cpal`, macOS only; elsewhere the card is
silent) reads it at the device's own rate and channel count through a linear resampler, so
opening the stream never retunes the host device. A host stream opens on the guest's PCM
START and closes on STOP or RELEASE; `-verbose` logs each with its frame counts (frames
played, frames above -80 dB, dropped, skipped, silence-filled). Capture records the default
input only with `-allow-host-audio`; otherwise it is silence. After the host deschedules the
card's thread for longer than a period, only one overdue buffer completes at once and the rest
follow at the stream's rate: returning every queued period in one burst left the guest's
driver queueing nothing more, and every HAL write then timed out (EIO).

Checked on API 36 `google_apis` (phone, `android.hardware.audio@7.1-impl.ranchu`): the card
is `/proc/asound` card 0 (`pcmC0D0p`, `pcmC0D0c`); AudioFlinger's primary output (48 kHz,
stereo S16, 1088-frame HAL buffer) plays through it, the HAL setting 544-frame periods in a
2176-frame buffer; a ringtone preview (`am start -a android.intent.action.VIEW -t audio/ogg
-d file:///product/media/audio/ringtones/Andromeda.ogg`) played on the Mac's speakers, about
11 s of it with no underrun on either side and no HAL write errors. Phone API 35 and 37
(`google_apis`), Wear (`android-wear-signed` 37) and TV (`android-tv` 36) boot with the card
(2026-11-25, release emu, `-no-audio -verbose`): each has `/dev/snd/pcmC0D0p`, its audio HAL
opens the playback stream (48 kHz stereo S16, 2176-byte periods in an 8704-byte buffer) for a
UI sound or a ringtone preview, and a minute of logcat after that has no audio HAL errors. The
looping ringtone preview on the phones logs `AudioTrack: pauseAndWait: timeout expired` (a
warning in the player's client, about every 3 s while it loops). Not yet booted with the card:
Automotive, XR; capture from a real microphone.

An I/O buffer may carry at most the stream's buffer and no more than its ring holds (1 s):
longer chains complete as a bad message without being read, so a guest can't make the card
copy, decode or allocate gigabytes.

**Needs a real boot.** Whether the car image reaches the launcher with these two pieces; that
the VHAL connects and `car get gear` answers; that the kernel's `virtio_snd` driver binds on
virtio-mmio and `caremu` opens card 0 (its mixer has no controls, which its init loop
tolerates); and the rest of the car stack (EVS, cluster display, rotary controller), which
simee doesn't model.

**Checked offline** against `system-images;android-34-ext9;android-automotive;arm64-v8a` r5
(`scripts/inspect-image.py`, `emu -avd Car34 -dry-run`): the image ships `build.prop` beside it,
so simee recognizes it as a car and adds `qemud:car` and the sound card; its first-stage fstab
and modules match the board; its VHAL (`android.hardware.automotive.vehicle@V1-emulator-service`)
opens `pipe:qemud:car`, over vsock first; and every property and gear/ignition value in
`sim_core::vhal` exists in the image's own `vehicle-types-meta.json` (a trimmed copy is a test
fixture). Its vendor also runs EVS, CAN, broadcast radio and remote-access HALs, none of which
simee serves.

## Performance notes

What has mattered so far, measured with `sample` and the `-verbose` exit counters:

- **GLES over the goldfish pipe (API 29–30): the renderer's own render channels.** These
  images' GLES encoder writes its stream to an `opengles` pipe per guest GL thread. The stock
  emulator serves that pipe from `libandroid-emu-shared` (`EmuglPipe`), not from the renderer
  library: `libgfxstream_backend` has no `opengles` service, and its exported
  `android_pipe_guest_*` API only reaches the services it registers (`GLProcessPipe`,
  `refcount`, adb), with no exported hook for host wakeups. What it does export is the class
  `EmuglPipe` sits on, `RenderChannelImpl` (constructor, `tryWrite`, `readBefore`,
  `waitUntilWritable`, `stop`, destructor): one channel is one render thread, and its two
  buffer queues take their own lock. emu constructs one per pipe
  (`crates/emu-gfxstream/src/channel.rs`), after checking the shape it relies on: the size
  `createRenderChannel` allocates, decoded from its code, and the vtable of what it built. Guest
  writes go into the channel from the vCPU (one copy into the channel's buffer); a reader thread
  per pipe sleeps in `readBefore` and hands replies to the pipe device, which wakes the guest.
  The alternatives were worse for a farm: virtio-gpu contexts with `PIPE_BUFFER` transfers
  (`VirtioGpuRenderThreadPipe`) read exactly the size asked, block for it, and must stay on the
  single thread that makes `stream_renderer_*` calls, so every guest process would wait on every
  other; loading `libandroid-emu-shared` would bring AppKit, Qt-era agents and the stock
  emulator's globals into emu. Like stock, each connection has its own render thread running in
  parallel; emu adds one sleeping thread per connection. `GLProcessPipe` tells the renderer when
  a guest process starts and exits (`FrameBuffer::createGraphicsProcessResources`,
  `cleanupProcGLObjects` on a cleanup thread), as the stock pipe does. The renderer offers these
  guests neither the virtio-gpu extensions (`VirtioGpuNext`, `VirtioGpuNativeSync`) nor shared
  host-memory slots: API 29's gralloc takes a shared slot for each color buffer update and fails
  on the large ones ("lockAndWriteDma: ERROR: No DMA context bound!"), so system_server's
  software-drawn windows never post and its watchdog restarts it; stock doesn't offer slots to
  these images either (its `HostComposition ext` line, 2026-11-26).

- **API 29's first boot runs the host GLES on SwiftShader (#94).** On ANGLE's Metal backend a
  fresh API 29 `default` AVD (or `-wipe-data`) stops before the launcher: `sys.boot_completed`
  comes, then system_server's watchdog fires and restarts it, again and again (5 of 5 first
  boots, 2026-11-26; warm boots of the same AVD reach the launcher). A guest `glClear` on a user
  framebuffer (a 16x32 texture attached at `GL_COLOR_ATTACHMENT0`, no EGL surface current;
  lldb on the faulting render thread, 2026-11-26) reaches
  `rx::FramebufferMtl::clearWithLoadOpRenderPassNotStarted` with a color attachment whose Metal
  texture is gone (`EXC_BAD_ACCESS` at `0x38`). gfxstream's Mach exception handler
  (`MacSegvHandler`) answers a fault outside its registered ranges with success, so the thread
  re-runs the faulting load forever at 100% CPU while holding ANGLE's global lock, and every
  other render thread waits on that lock; the composer HAL and bootanimation then spin on their
  `O_NONBLOCK` `opengles` pipes (36.5M goldfish-pipe `CMD` writes in 40 s), a symptom. The fault
  is timing-dependent: with lldb attached from launch the same first boot reached the launcher
  (2 of 2), and the texture and framebuffer are the guest's Skia objects, which emu doesn't
  create or size, so emu has no input to change; the bug is in the SDK's ANGLE build
  (`fbf66f49c7cc`). Stock renders a headless first boot with `swangle` (ANGLE on SwiftShader)
  too. So emu runs ANGLE on SwiftShader for an API 29 image's first boot (no `userdata.img`
  yet, as stock tells a first boot) and on Metal afterwards; `-verbose` says so.
  `ANGLE_DEFAULT_PLATFORM` overrides it (`metal` reproduces the hang). Times to the launcher are
  in [benchmarks](benchmarks.md#api-29-gles-over-the-goldfish-pipe). API 30 `google_apis` and
  API 36 first-boot on Metal to the launcher.

- **API 29–30's reference RIL gets the stock modem's answers (#83).** These images' `rild`
  (the goldfish `libreference-ril`, over `qemud:gsm`) polls `AT+CREG?`, `AT+CGREG?` and
  `AT+CTEC?` and reads a fifth `+CREG` field as the voice radio technology, an Android
  `RIL_RadioTechnology`. emu answered as it answers the radio HAL, with TS 27.007's `<AcT>`
  (`7`, E-UTRAN), which the RIL read as `EVDO_0`: the phone switched to CDMA, the next GPRS
  report switched it back, and the device idled at 149–156% of a host core with
  `com.android.phone` and `system_server` busy. Once a guest opens `qemud:gsm`, the modem answers
  in stock's formats (`crates/emu-vmm/src/modem/mod.rs`): `+CREG` without a technology,
  `+CGREG`/`+CEREG` with the Android one in hex (LTE `000e`), and `+CTEC` with technology
  indices (emu's own values, not stock's GSM-only ones). Idle CPU fell to 2.9% and 2.1% of a core (stock: 5.4% and 3.7%;
  [benchmarks](benchmarks.md#api-29-and-30-first-boots-and-idle-host-cpu)).

- **In-kernel GICv3.** The stock board emulates a GICv2 in userspace. Here interrupt
  acknowledge, EOI and the virtual timer never leave the guest, and an idle vCPU parks inside
  the framework (zero WFI exits in practice).
- **No kick on interrupt (macOS 26 and later).** An earlier macOS left a vCPU idling inside the framework asleep
  until its own timer fired when a device raised an SPI, and kicking vCPUs out of
  `hv_vcpu_run` after raising one cut first-boot `/data` encryption from 22 s to 8 s. On macOS
  26.6 `hv_gic_set_spi` alone wakes the target, as other VMMs on macOS 26 found, and the kicks
  only cost exits. The guest routes every SPI to vCPU0 (Android has no irqbalance), so each
  device completion cost vCPU0 an InterruptStatus read, an InterruptACK write and a kick, and
  the kicks were 28% of its guest entries. A/B on an API 36 phone (720×1280, 4 vCPUs), M3 Max,
  macOS 26.6.2, 2026-11-26, interleaved, `scripts/android-bench.sh`, median boot to
  `sys.boot_completed` (runs), targeted kicks → none. The first-boot row is the runs at a 1-minute
  load of 7.7 to 8.1 on 16 CPUs; an earlier set at load 9.5 to 20 had starved runs in it and is
  not used. The warm row was measured at load 9.6 to 13.2.

  | Boot | Targeted kicks | No kicks | vCPU0 guest entries at boot | emu CPU-s at boot |
  | --- | --- | --- | --- | --- |
  | First (`-wipe-data`: format, `/data` encryption, dexopt) | 11.4 s (11.4, 11.5, 11.4) | 11.6 s (11.8, 11.4) | 348k → 257k | 21.7 → 22.1 |
  | Warm | 4.2 s (4.7, 3.5, 3.6, 3.9, 4.6, 4.4) | 3.7 s (3.7, 3.7, 3.7, 3.7, 5.2, 4.8) | 59.7k → 41.4k | 9.3 → 8.7 |

  First boot is a tie: no kicks is 0.2 s slower on a median of two, inside the spread, with a
  quarter fewer vCPU0 guest entries. Owed: a third low-load `none` run and more of both (the host
  load climbed past 14 during the rest of the rerun, and those boots, 12.6 to 22.8 s, are not
  published). The warm-boot, vCPU0-entry and jank rows are what the default rests on. The
  default is chosen from the macOS version (`Mode::default_for_macos`): `none` on 26 and later,
  `targeted` below 26 and when the version can't be read, because the 22 s to 8 s stall was
  seen on an earlier macOS and nothing here measured those.

  Without kicks vCPU0 had 1 canceled exit per boot instead of about 16k (warm) or 100k (first).
  Launcher swipes (`scripts/android-jank.sh`, a minute after boot, 2 boots per arm, 3 runs
  each, interleaved, host load 5 to 11) didn't move: janky frames median 0.4% with kicks and 0.2%
  without, p90 5 ms in both, emu 28% and 25% of a core. (At host load 16 to 54 both arms were
  janky, 2–66%, in step with the load, not the arm.) No boot or frame got slower, so `none` is
  the default; `EMU_VCPU_WAKE=targeted` brings the targeted
  kicks back (only the SPI's `GICD_IROUTER` target, only while it is in the guest, once per
  entry; `crates/emu-vmm/src/wake.rs`) and `EMU_VCPU_WAKE=all` the old kick-everyone. Moving
  SPIs off vCPU0 or suppressing more notifications would not pay yet: the MMIO handlers took
  0.15 s of vCPU0's 20.7 s over a boot and 15 s of use (`EMU_EXIT_PROFILE=1`), and blk requests
  arrive one at a time, so each is one QueueNotify and one interrupt with or without event-idx.
- **Where a boot's host time goes** (same phone, `sample <pid> 5` during boot and during
  `scripts/android-jank.sh`, 2026-11-26). Nearly all of emu's CPU is the guest running inside
  `hv_trap`; VMM code (MMIO handlers, `preadv`/`pwritev`, the wake bookkeeping) was under 2% of
  the samples. Ranked by what a user waits for:
  1. *The renderer's Metal shader compiles.* With Metal's cache warm the init takes 0.33 s;
     cold it took 4.5 and 5.1 s (2 of 20 launches that day), and the boot waits for it. Mostly
     ANGLE's own Metal shader library, which a renderer without GL no longer compiles (host
     ANGLE on SwiftShader, cold 1.5 s), and why it went cold: "Renderer init and Metal's shader
     cache" below.
  2. *The renderer's ring polling.* gfxstream's render threads yield-spin on the
     address-space-graphics ring (`RingStream::readRaw` → `cthread_yield`) while the guest
     draws. Measured again after the Vulkan-only renderer: about 1% of a core; see "Ring
     polling" below.
  3. *Interrupt round trips on vCPU0*, now without the kicks (above): three exits per block
     request, 0.15 s of handler time per 20 s.
  4. *Teardown.* A stop takes 0.2–0.6 s; releasing the userdata disk took 0.3 s once (its
     worker's last writes). `-verbose` prints every teardown step over 250 ms, and a stop that
     overruns 5 s says which step it was waiting for and prints the exit counts before exiting.
- **Plain `fsync` for guest flushes.** `std`'s `sync_data` is `F_FULLFSYNC` on macOS, which
  drains the drive cache. It was 75% of the userdata disk thread's time, and APEX
  decompression on first boot took more than 270 s; with plain `fsync` it takes about 70 s.
  Writes already sit in the host page cache, so an emulator crash loses nothing either way.
- **Zero-copy disk I/O.** `pread`/`pwrite` go straight between the image and guest RAM, one
  worker thread per disk, so the vCPU that kicked a queue goes back to the guest at once.
- **One syscall per block request, no kicks while draining.** Linux sends a 128 KiB request as
  one indirect table of up to 32 scattered pages; virtio-blk issued a `pread`/`pwrite` per page.
  Now a request is one `preadv`/`pwritev` (guest-adjacent pages merged, at most 1024 iovecs a
  call), an ephemeral disk checks its overlay once per request, and a whole-chunk write to an
  ephemeral disk no longer reads the base chunk it replaces. In-process A/B medians on a loaded
  Mac (`cargo test --release -p simee-emu-vmm disk_vectored_vs_per_segment -- --ignored`), per
  request: persistent read 20 → 7.2 µs, persistent write 62 → 9.9 µs, ephemeral read 20 → 7.5
  µs; the skipped base read takes ephemeral writes to fresh chunks from 16 to 10.5 µs. The
  worker turns notifications off while it drains and back on with a recheck (QEMU's
  `virtio_queue_set_notification`; publish `avail_event`, full barrier, reread `avail.idx`), so
  a guest that publishes while a request is being served doesn't kick. With a driver thread
  racing the worker (`blk_kicks_while_draining`), kicks per request at queue depth 2 went from
  1.0 to 0.001 with event-idx, and at every depth ≥ 2 from 1.0 to under 0.005 without it; at
  depth ≥ 4 with event-idx the old code already rarely kicked. Each saved kick is an MMIO exit
  on a real guest. Popping a 34-entry indirect chain into a reused `Chain`, with the table read
  in one copy, takes 82 ns instead of about 400 (`queue_pop_indirect`).
- **Sparse, lazily committed memory.** Guest RAM is an untouched anonymous mapping and the
  userdata image is a sparse file.
- **Discard punches holes.** virtio-blk `DISCARD`/`WRITE_ZEROES` become `F_PUNCHHOLE` on
  persistent images (an in-memory zeroed-range set on ephemeral ones), so space the guest frees
  goes back to the host: with `/data` mounted `-o discard`, writing then deleting 512 MiB grew
  the image by 525 MB and shrank it by 524 MB. A whole-disk FITRIM takes 90 ms. `/data` sits on
  dm-default-key metadata encryption, which passes discard through but not write-zeroes, and
  zeroes it writes land as ciphertext. So discard is the only way to free space, and a fresh
  first boot (about 610 MB allocated) has only about 2 MB left to trim.
- **AVD disks are copy-on-write clones (#203).** Every disk an AVD is made from (a golden's,
  `-clone-avd`'s, `boot.prop`) goes through one helper, `emu_avd::clone::clone_file`:
  `clonefile(2)` on APFS, `FICLONE` on Linux file systems with reflinks, else a copy of only the
  data extents (`SEEK_DATA`/`SEEK_HOLE`), keeping the source's mode and extended attributes. On
  one APFS volume it was already a clone (`std::fs::copy` tries `fclonefileat` first):
  `-clone-avd` of an AVD with a 6 GiB userdata takes 11 ms and no disk before and after, and
  `-create-avd ... -no-golden` 11 ms and 20 KiB (only the ini files; median of 5, M3 Max, macOS
  26.6.2, 2026-11-27, host load about 7). What changed is the fallback: from an AVD whose disks
  are on another APFS volume (`EXDEV`; a 6 GiB userdata holding 1 GiB of data), `std::fs::copy`'s
  `fcopyfile` filled the holes, 2.97 s and 6.1 GiB per clone; the extent copy takes 0.57 s and
  1.0 GiB (median of 5, same host and day).
  The key is the one disk written at boot, so `emu_avd::clone::clone_file_atomic` copies
  `encryptionkey.img` under `.encryptionkey.img.partial` and renames it into place: a kill mid-copy
  leaves no half-written key at the final name. A symlink at that path, dangling or not, is
  refused with an error naming it (the old `std::fs::copy` wrote through it). The tests run the
  real `FICLONE` where the file system has it; `SIMEE_REFLINK_DIR=<dir on Btrfs or XFS>` points
  them at one (a loop-mounted `mkfs.xfs -m reflink=1` image works).
- **Screenshots: fast PNG, no re-encode, no needless copies.** Automation takes a screenshot
  after nearly every step. On a 2480×1740 screen (`cargo run --release -p simee-core --example
  frame_bench`), PNG encoding at the `png` crate's default level took 74 ms and BGRA→RGBA
  conversion 11 ms; `Fast` (fdeflate) encodes in 11.5 ms for a file 1.7× larger, and a conversion
  loop per pixel format (no per-pixel `match`) takes 7.8 ms: 86 → 19 ms. The API passes the
  emulator's PNG straight through (with `path`, emu writes the file itself) instead of decoding
  and re-encoding it, and the display socket's frame loop no longer clones the whole frame just
  to read its size. The display socket's per-frame downscale (a box filter) went from 22.7 to
  7.2 ms at factor 2 (16.4 to 7.5 ms at 3): const-generic kernels for factors 2–4 and division by
  a reciprocal, identical output.
- **The frame path: no copy per post, one readback per frame, a shared-memory ring.** The path
  is described in [the display design](design/desktop-and-display.md#frame-path-built). Stage by
  stage for the desktop's stream (`hello 1080`) on a 4-core Linux VM (memcpy 5.4 GB/s), medians,
  tablet 2480×1740 / phone 1080×2400:

  | Stage | Before | After | Benchmark (`--ignored --nocapture`) |
  | --- | --- | --- | --- |
  | Post callback (renderer thread) | 3.9 / 1.6 ms copy | 0.0002 ms (size only) | `cargo test --release -p simee-emu-vmm present_cost` |
  | Readback | per stream and per screenshot: Vulkan → CPU → GL → CPU under the FrameBuffer lock | once per frame, shared by streams and screenshots (a full-size ring stream reads straight into its slot): Vulkan → CPU | needs a Mac (below) |
  | Scale (/3 / /1) | 8.0 ms / 1.8 ms copy | 2.0 ms (2.8 on one thread) / none | `cargo test --release -p simee-core downscale_cost` |
  | Emulator → socket (1.9 / 10.4 MB) | 0.86 / 3.7 ms of writes | a 32-byte notice | `cargo test --release -p simee-emu transport_cost` |
  | Emulator CPU per frame besides the readback | ≈12.8 / ≈7.1 ms | ≈2.0 / ≈0 ms | sum of the above |
  | Post → client holds the pixels | — | 8.1–9.0 / 8.0–8.8 ms (ring), 7.8–10.3 / 8.5–9.3 ms (socket), across runs | `CASE=2480,1080,true cargo test --release -p simee-emu frame_path_latency` |

  End to end the ring and the socket tie on this VM: the latency is mostly two memory-bound
  copies, the stand-in readback (a memcpy of the frame) and the client's copy into the buffer it
  hands to `Pixels` (its `pread` costs what a socket read did). What the ring removes is the
  emulator's work: no payload writes, and frames are scaled (or read back) straight into shared
  memory. A new ring is written out in full when it's made (so a full disk is an error rather than a
  `SIGBUS`), which costs its first frames 20–60 ms here, once per ring. Frames scale on one thread below 16 MiB of input and in up to 4
  bands above: two threads were slower than one on this VM (likely SMT siblings), four faster.
  Rings live in `<avd>/simee/`, a disk-backed file on macOS whose dirty pages the system writes
  back periodically (benchmark rigs on Linux use `/dev/shm`). With a GL backend the direct Vulkan
  readback is checked against the GL one on its first 3 frames and turned off if they differ
  (`EMU_READBACK=gl` turns it off outright); a Vulkan-only renderer has only the direct one.
- **Vulkan-only renderer.** With a GL backend, gfxstream gives every color buffer a GL texture
  beside its Vulkan image, and after each guest queue submit copies the image into it on the CPU:
  a Vulkan readback, then `glTexSubImage2D`, which Apple's driver retiles on the CPU
  (`agxsTwiddleAddressCommon` was a fifth of a core in `sample` while a tablet animated). Two
  full-frame copies per frame, so the cost grows with the screen: 16 MB each on a 2560×1600
  tablet, whose launcher missed 37% of its frames. The guest's GLES runs on ANGLE over its own
  Vulkan, so nothing reads the GL side, and emu starts the renderer without GL (gfxstream's
  `GuestVulkanOnly`): no GL texture, no copy. Frame pacing then beats stock on the phone and the
  tablet ([benchmarks](benchmarks.md#frame-pacing)). Two things change with it. gfxstream's post
  callback needs GL (registering it and every post read back through GL), so a `PostTicker`
  stands in: each renderer fence pokes it, and while they come it marks a new frame for the
  display store at most every 16 ms, and for 50 ms after the last, which covers the post the
  renderer's post worker makes after the composer's fence; a quiet guest costs nothing. And host
  frames are read only by the direct Vulkan readback, so emu chooses Vulkan-only only when the
  renderer build has it. `EMU_GUEST_EGL=emulation`, `EMU_GL_COMPOSITION` and `EMU_RENDERER_GL=1`
  keep GL. Host screenshots matched the guest's `screencap` on every form factor, the stream ran
  40 fps at full tablet size during swipes (GL: 24–33), and the AutoMobile harness gave the same
  results under both on phone, tablet, foldable, watch, TV, car, XR headset and glasses.
- **A turned device: frames and touches follow the guest's display rotation, turned where it's
  cheapest, never when upright.** The panel never turns; a guest that rotates draws its turned
  layout sideways into the same scanout. Whether it rotates is the guest's call: with
  auto-rotate off, with a portrait-locked app (a phone's launcher) and when a phone is turned
  upside down (it refuses 180°, staying in landscape) the accelerometer turns and the display
  doesn't, and a device lying flat turns nothing. So emu turns frames and touches by the
  rotation the guest reports, not by the accelerometer: each rotation is logged at info, as
  `WindowManager: Display id=0 rotation changed to N` (API 36 and later) and as
  `ActivityTaskManager: Config changes=... mDisplayRotation=ROTATION_N` (API 33 and later), or
  `mRotation=ROTATION_N` in the same line where `mDisplayRotation` isn't (API 31 and 32; checked
  under simee on API 31, 33, 34 and 36, 2026-11-26; on API 33 with auto-rotate on (`simee rotate`) and off (`wm user-rotation`; `simee rotate` left the locked guest upright), `simee screenshot` matched `adb screencap` in each turn, console `rotation` reported it and a `simee tap` at a uiautomator element's centre opened it), and emu asks the guest's `goldfish-logcat` for
  those two tags at info (`androidboot.logcat`, beside whatever `-logcat` asks for, which still
  sees only its own lines) and reads them off the logcat console as they arrive
  (`sim_core::rotation::guest_rotation`, only under those two tags, so an app logging the same
  words turns nothing; no polling and no adb). The reader runs inline on the console worker,
  before the spool that feeds `-logcat`'s output (a utility-priority thread that drops chunks
  when it falls behind), so a slow or verbose `-logcat` never loses a rotation line. It is
  still a log: a rotation reaches emu a few milliseconds after the guest relayouts, so a tap at
  bounds read from the turned layout and sent in that window maps with the old turn; a caller
  that rotates then taps should read `rotation` (the console, or `simee screen`) until it shows
  the new turn. The two tags add little log: on an API 36 phone (720x1280, 2026-11-26, this Mac at load 19 to 62), 4.3 KB in 17 lines from boot to boot complete, nothing in 2 minutes idle, and 11.4 KB in 50 lines over four rotations and five Settings launches (2 rotation lines each), against 1.9 MB in 10,385 lines that the guest logged under all tags in the same window (`adb logcat -d -s WindowManager:I ActivityTaskManager:I`; one run, not a benchmark). API 29 and 30 aren't followed: their filter
  goes through `qemu.cmdline`, at most 91 bytes, which the two tags would overflow, so emu
  leaves it as the user's and `rotation` says `unknown` there (frames and touches stay as the
  panel lies). Phones, tablets and foldables follow it; TVs, cars, watches and XR headsets and
  glasses never turn (`sim_core::rotation::turns`). A guest reset starts upright again. Screenshots,
  frames, the API's `screen`, `tap`, `swipe`, `touch`, `stylus` and the console's `touch` and
  `pen` are then all in one space, the guest's layout: what `adb screencap` and uiautomator's
  bounds are in, in every case above (the console's `rotation` says how far it is turned).
  Stock differs here: its window and its gRPC and `screenrecord` screenshots follow the
  accelerometer's coarse orientation, so with auto-rotate off or after a phone's 180° they show
  the guest's layout sideways while `adb screencap` doesn't (checked on 37.2.13). The readback is a renderer copy into host memory with no turn of its
  own, so the turn happens after it, once per frame, and only for readers that need it: the
  desktop sends `rotates` and gets frames as the panel lies with an `EMUO` record saying how far
  to turn them, and Rune turns the picture as it draws (a GPU transform; no CPU turn anywhere);
  screenshots and other stream clients get the frame turned upright, from a turned copy the
  store makes once per frame and shares (1.2–1.4 ms for 1080×2400, 1.6–1.9 ms for 2560×1600 on
  this Mac, 32-pixel tiles: `cargo test --release -p simee-core rotate_cost -- --ignored
  --nocapture`), which goes with the other buffers when nobody reads. Upright, readers get the
  capture itself and a full-size ring stream still reads straight into its slot; a unit test
  pins that no turned buffer is made. Touches on the display socket and the console's `touch`
  and `pen` are in the upright screen's (the guest layout's) pixels; emu maps each onto the panel pixel shown there
  (and a folded foldable's view origin), which is what the touch screen, fixed to the panel,
  reports. Stock's gRPC touches, by contrast, stay in panel pixels when turned (checked on
  37.2.13: a `sendTouch` at 300,900 on a turned phone reached the guest as panel 300,900).
- **Gboard's blank keyboard: the UI draws with Skia on Vulkan, as stock's does.** On MoltenVK
  Gboard's keyboard was one flat panel: its buffer came out fully transparent (guest `screencap`
  and host frame alike), on 3 of 3 boots; on KosmicKrisp it drew. emu ran every app's HWUI as
  Skia GL on ANGLE on the guest's Vulkan, and that stack on MoltenVK loses Gboard's drawing.
  Stock 37.2.13 with `-gpu host` drew it, but there the guest's GLES is the `emulation` encoder,
  not ANGLE, so it was not the same stack. Stock with `-feature GuestAngle` (emu's stack) draws it
  too, and its `/proc/bootconfig` showed why: on an API 36 image a `GuestAngle` guest gets
  `androidboot.debug.hwui.renderer=skiavk`, `androidboot.debug.renderengine.backend=skiavkthreaded`
  and `androidboot.hardware.angle_feature_overrides_disabled=exposeN*`, and the renderer runs with
  `VulkanVirtualQueue`, without which HWUI on Vulkan aborts every app (`VulkanManager::initialize`
  asserts its queue family has two queues). Only images that advertise `VulkanVirtualQueue` (API
  36 and newer) take that: API 34 and 35 map `ro.boot.debug.hwui.renderer` but not RenderEngine's
  backend (35 hard-codes `skiaglthreaded`), and on google-xr 34, which enables `GuestAngle` but
  not `VulkanVirtualQueue`, stock gives `hwui.renderer=skiagl` and the `exposeN*` overrides and
  leaves RenderEngine alone. emu now passes the same per image, so on API 36 and newer apps and
  SurfaceFlinger draw with Skia on Vulkan and only apps' own GLES goes through ANGLE; API 34 and
  35 phones and Automotive (neither feature) keep Skia GL on ANGLE, and there the bug remains:
  on an API 34 phone Gboard's keyboard and the Settings search page behind it still came out
  blank (one boot). In the recorded A/B on an API
  36 phone (consecutive boots at host load 5), the build before stayed blank on 2 of 2 boots and
  this one drew Gboard on 2 of 2 (`Pipeline=Skia (Vulkan)`), guest `screencap` and host
  screenshot alike. Launcher frame pacing didn't move: `scripts/android-jank.sh` on that
  1080×2400 phone, 3 runs per boot, janky frames median 0% before and after, p90 5 ms, emu 21% →
  22% of a core. A second pair ran while other work loaded the host to 21–37; both arms were
  janky (1–20%) at different loads, so it isn't compared.
- **vsock without intermediate copies.** adb (and every qemud pipe) rides vsock. Host bytes were
  copied into the connection's queue, out of it into a packet `Vec`, and then into the guest's
  buffer; now the adb forwarder hands its buffer over, the worker takes it over when nothing is
  queued, and packets are written straight from the queue into the guest's descriptors
  (`Chain::write_parts`). Host→guest through one pipe with the guest posting Linux's 4 KiB
  receive buffers (`cargo test --release -p simee-emu-vmm vsock_throughput -- --ignored`): median of 5,
  284 → 512 MiB/s on a 4-core Linux VM. Guest→host (`vsock_throughput_to_host`, 64 KiB
  packets) runs at about 4 GiB/s, so host→guest's limit is the guest's small receive buffers.
- **vsock backpressure: a stalled client stalls only itself.** One worker thread serves every
  pipe, and the adb forwarder used to `write_all` to its client on it, so an adb client that
  stopped reading (`kill -STOP`) froze sensors, GPS, the vehicle HAL and every other adb session
  once its socket filled. Now the worker advertises `fwd_cnt` only for bytes a pipe has
  delivered (`PipeHandler::backlog`), as Firecracker and Cloud Hypervisor do, and sends the
  credit update itself, because Linux guests never send CREDIT_REQUEST. The forwarder sends
  without waiting and queues the rest for a writer thread, at most the 256 KiB the guest was
  allowed. A guest that sends past its credit is reset, as Linux resets one. The other
  direction is bounded too: a client is read only while less than 1 MiB waits for the guest
  (`guest_took`). The stock emulator's vsock path buffers without limit both ways. Connections
  take turns for rx buffers (a ready queue rather than a scan per buffer), and tx waits while
  a queue's worth of replies waits for rx buffers, as vhost-vsock does. Medians of 10
  interleaved runs on a 4-core Linux VM, before → after: `vsock_throughput` 474 → 597 MiB/s,
  `vsock_throughput_to_host` 5.9 → 6.4 GiB/s (its guest now honours credit),
  `adb_throughput_to_guest` (TCP client → guest, end to end) 498 → 870 MiB/s,
  `adb_throughput_to_host` 1382 → 1433 MiB/s.
- **Renderer init and Metal's shader cache.** A cold renderer init is about 4.3 s of Metal shader
  compiles and a warm one under 0.1 s (`cargo run --release -p simee-emu-gfxstream --example
  renderer_caps -- -emu <lib64>`, which inits as an API 36 boot does: cold 4.31, 4.36, 4.39 s,
  warm 70 to 96 ms, M3 Max, 2026-11-26). `sample` of a cold init: the time is ANGLE's Metal
  backend compiling its built-in shader library (`DisplayMtl::getDefaultShadersLib`, waited on
  by the first context's `ProvokingVertexHelper`), which gfxstream sets up even without a GL
  backend (`EmulationGl::initDispatchers` → the EGL display ANGLE provides); the compositor's
  Vulkan pipelines through MoltenVK are the remaining ~0.9 s. Why it goes cold: Metal keeps its
  cache per bundle identifier, in `$DARWIN_USER_CACHE_DIR/<bundle id>/com.apple.metal/`, and a
  process without one (emu was one) uses the single cache under
  `$DARWIN_USER_CACHE_DIR/com.apple.metal/` that every unbundled process of the user shares:
  CLI tools, test binaries, every lane's emu. On this host that cache held 460 MB in 8 rotating
  shards and rotated several times a day; a fixed `emu` binary that had initialized warm went
  cold again within minutes (4.69 s and 1.01 s inits in 2 of 3 repeat boots of one binary that
  had just initialized warm, and 2 of 3 launches of the probe binary in another 10-minute
  window; [benchmarks](benchmarks.md#emulator-internals)). With its own cache, 4 of 4 such
  boots initialized in 0.33 to 0.36 s and reached `sys.boot_completed` in a median 7.8 s instead
  of 11.7 s (7 boots in all; a third interleaved round had the shared cache warm in its 3 before
  boots too, 7.8 s median, so the gain is the misses, which come and go with what else runs). Entries also seem tied to the executable's location: the first launch of the same
  binary copied or moved to a new directory was cold in 4 of 6 tries (3.86 to 5.42 s, then
  warm), while touching, re-signing, symlinking or another cwd kept it warm; what else keys
  them wasn't established. So `emu` now carries an `Info.plist`
  (`__TEXT,__info_plist`, `apps/emu/build.rs`) naming bundle `dev.simee.emu`: its shaders sit in
  a cache only emu writes (the renderer's init, guest shaders), every emu build and instance at
  one path shares it, and other processes can't rotate it out. And `emu -provision-renderer`
  (and `-provision`) end by initializing the renderer in two children, the probe's GL-backed
  init and an API 36 boot's own (`emu_vmm::warm_renderer`), so the first boot of a fresh install
  finds the cache warm (0.9 s for both when warm). Whether that pays on a fresh install isn't
  established yet: a fresh copy's boot-shaped init took 0.78, 1.00 and 0.78 s after
  `-provision-renderer` from its path, against 5.42, 0.83 and 0.73 s without (`-warm-renderer`
  timed, host load 180 to 210), a win in only one of three rounds; the quiet-host rerun is under
  "Not yet measured" in [benchmarks](benchmarks.md). Still cold
  once, sometimes: the first launch from a new path without provisioning there, and, not yet seen, likely the first after a macOS update
  (the cache's subdirectories look keyed by compiler and GPU: `32024`, `16777235_419`).
  KosmicKrisp wasn't measured.
  A renderer without GL (guests on ANGLE, API 34 and newer) doesn't need that library at all:
  nothing draws with the host's GLES, yet gfxstream opens ANGLE's display at init anyway
  and makes one context there (`EglOsEglDisplay`), and on Metal that context's
  `ProvokingVertexHelper` waits for ANGLE's whole built-in shader library. So emu runs the host
  ANGLE on SwiftShader whenever the renderer will run without GL (`emu_vmm::runs_without_gl`,
  which reads the renderer file's direct-readback symbols before it loads, since
  `ANGLE_DEFAULT_PLATFORM` must be set while the process is single-threaded), as it already did
  for an API 29 first boot; `ANGLE_DEFAULT_PLATFORM=metal` brings Metal back. ANGLE's display then
  takes about 30 ms and compiles no Metal shaders, and what a cold cache still costs is the
  compositor's MoltenVK pipelines (`CompositorVk::setUpGraphicsPipelines`, about 0.6 s of the
  rest, in `sample`). Cold, the boot-shaped init went from 4.59 to 1.54 s and an API 36
  phone's boot from 15.06 to 12.10 s; warm, both are unchanged (0.35 s, 7.45 s), as is frame
  pacing during swipes, and the guest still draws on MoltenVK. A cold boot still takes about
  4.6 s longer than a warm one, and only 1.2 s of that is the renderer's init. Numbers in
  [benchmarks](benchmarks.md#emulator-internals). Guests on the GLES encoder (API 31 to 33) keep
  ANGLE on Metal, since their GLES runs there, and so keep the cold 4 s.
- **Ring polling.** When its address-space-graphics ring is empty, `RingStream::readRaw` polls
  it with `ring_buffer_yield()` (`sched_yield`) 30 times per round for 8 rounds, then marks the
  ring as needing a notification and blocks until the guest pings (disassembly of the SDK's
  37.2.13 renderer; the counts are immediates). No renderer flag, feature, `stream_renderer_init`
  parameter or environment variable selects another wait. With the Vulkan-only renderer the
  polling is small: in `sample` of the API 36 phone during launcher swipes, 43 and 51 of about
  4,400 samples per thread were in `swtch_pri` (about 1% of a core) out of 30 to 45% for the
  whole process. The renderer links with a flat namespace, so an executable can supply its own
  `ring_buffer_yield()`; emu tried one that skips the system call for the first 256 calls of a
  streak, and it changed neither emu's CPU during `scripts/android-jank.sh` swipes (30 and 38%
  of a core with it, 28 and 38% without, one run on each of two boots per arm, host load 9 to
  22) nor what `sample` showed (51 samples in `swtch_pri`, now under emu's function), since
  most polling streaks run past the first episode's 240 calls. It was dropped: the renderer's internals would change for no
  measured gain. The SDK's 37.2.13 renderer lists no ring or wait feature either (its
  feature list in `emu -verbose`). Measured again on 2026-11-27 (`sample` during swipes, API 36
  phone): the render threads spend about 0.5% of a core yielding in `readRaw`, and they spend the
  rest of their waits blocked in `MessageChannelBase::beforeRead`. Polling less would mean
  changing `RingStream::readRaw` in gfxstream's source.
- **Launch to first guest instruction.** Most images ship a gzip `kernel-ranchu` (an API 37
  kernel is 16 MiB that inflates to about 40 MiB). It used to be read whole, inflated into a
  `Vec` with miniz_oxide and copied into guest RAM on every launch. QEMU, and so the stock
  emulator, inflates on every launch too, while Firecracker and crosvm take only raw images. Now
  the first launch inflates it in 1 MiB chunks straight into guest RAM with zlib-rs and writes
  the image to `~/Library/Caches/simee/kernel-<path>-<stamp>`, keyed by path, size, mtime,
  inode and the gzip trailer's CRC32 and ISIZE (one 8-byte `pread`, so a kernel rewritten in
  place with its mtime kept is a miss), through a temp file that is synced before the rename. A
  hit must be exactly ISIZE bytes long: a short entry would read as zeros past its end. Older
  versions of the same path, and temp files over a minute old (a launch killed mid-write), are
  removed. Later launches `pread` that file into guest RAM, as they do a raw `Image`. Medians of
  7 interleaved runs on an M3 Max under CI load (`cargo test --release -p simee-emu-vmm kernel_load --
  --ignored --nocapture`): 115 ms before (miniz_oxide), 71–76 ms to inflate with zlib-rs, 86–95
  ms on a miss (the synced cache write adds 15–20 ms), 3.5–3.9 ms on a hit. The gfxstream
  renderer's load and init (about 400 ms, `renderer_init_off_thread`) now starts on its own
  thread once the console port is bound and the boot files are found, and `run` waits for it
  just before it creates the GPU device; a launch that fails before that point waits for the
  init to finish before it exits, so the process never exits mid-dlopen. The thread stays
  parked afterwards, as the main thread did. Reading the 1.8 MiB ramdisk straight into guest RAM instead of through a `Vec` saved
  0.06 ms, so it was left as it was. `-verbose` prints the kernel source and time, how long the
  boot waited for the renderer, and when the vCPUs started relative to launch.
- **Quiet console.** The PL011 traps twice per byte, and a boot printed ~230 KB on it: 88% of
  all vCPU exits. Without `-show-kernel` the cmdline now adds `loglevel=3`: only emergencies,
  alerts and criticals print, and dmesg still has everything. Stock passes `console=0` then and
  prints nothing. Measured with `EMU_EXIT_PROFILE` (`scripts/farm-bench-android.sh`, details in
  docs/farm-android.md): exits to boot_completed 516k → 77–109k, idle 4.5k → 122–154 exits/s.
- **goldfish-logcat.** `init.ranchu.rc` runs `logcat -f /dev/hvc1` all the time. hvc1 had no
  backend, so logcat restarted every ~5 s. The virtio-console's third port (unnamed, so a console
  port, which makes it hvc1) now backs it. Its output is discarded with filter `*:S` unless
  `-logcat TAGS` or `-logcat-output FILE` is given (through a bounded queue that drops rather
  than stall the console worker; FILE rolls over at 4 MiB). sim-android starts emu with
  `-logcat '*:I' -logcat-output <avd>/simee/logcat.txt`, which the desktop's Android panel shows.
- **Giving guest memory back to the host.** `GuestRam::discard` uses `hv_vm_unmap`, then
  `mmap(MAP_FIXED)`, then `hv_vm_map` on whole 16 KiB pages. It is the only method measured that
  both lowers `phys_footprint` and reads back as zeroes. Measured with a VM in a test (64 MiB,
  half written by the host, half by the guest; Apple M3 Max, macOS 26.6.2, 2026-11-24; run each
  with `cargo test -p simee-emu-hvf --test discard_probe -- --ignored --nocapture --exact <name>`):

  | Method | Footprint | Guest reads after | Host reads after | Host and guest share pages after |
  |---|---|---|---|---|
  | `madvise(MADV_DONTNEED)` | 66.1 → 66.1 MiB | old data | old data | yes |
  | `madvise(MADV_FREE)` | 66.1 → 66.1 MiB | old data | old data | yes |
  | `madvise(MADV_FREE_REUSABLE)` | 66.1 → 34.1 MiB | old data | old data | yes |
  | `mmap(MAP_FIXED)` alone | 66.1 → 34.1 MiB | old data | zeroes | **no** |
  | `hv_vm_unmap` + `mmap(MAP_FIXED)` + `hv_vm_map` | 66.1 → 2.0 MiB | zeroes | zeroes | yes |

  - The shipped method costs 30–80 µs per MiB and 0.6–1.9 ms for 32 MiB (`guest_ram_discard_in_a_vm`).
  - A page that both the host and the guest touched counts twice in `phys_footprint`: the host
    reading the guest-written half raised it from 66 to 98 MiB, with nothing discarded.
  - The goldfish address space's clearing thread now discards freed runs of 1 MiB or more. In a
    model of the device (no guest), freeing 64 MiB of buffers left 64 MiB resident before this
    change and 0 MiB after (`cargo test -p simee-emu-vmm --lib clear_cost -- --ignored --nocapture`).
    Not yet measured with real gralloc traffic.
  - A virtio-balloon or free-page reporting would have to use the same unmap and remap path.
    `madvise` alone either keeps the old data or doesn't lower the footprint of pages the guest
    touched.

- **Thread QoS.** A thread doesn't inherit its spawner's QoS class on Darwin, so emu sets one on
  each thread: user-initiated on vCPUs and device workers, utility on housekeeping (signal
  handling, adb announcements, the logcat writer, the address-space clearer). Lowering works as
  documented: utility threads run at priority 20. Raising doesn't always: launched from an agent's
  shell on this Mac (M3 Max, macOS 26.6.2, 2026-11-24), threads set to user-interactive,
  user-initiated and default all ran at priority 31 (`ps -M`), so the process's QoS ceiling was
  default there. Whether vCPUs reach a higher tier when emu is started from Terminal or a
  launchd job, and whether that changes boot or settle time, is not yet measured.

- **GPU state across guest resets.** The renderer is process-wide and outlives each boot, but a
  reset left the guest's virtio-gpu contexts and resources in it. An orderly `adb reboot` hid
  that, since the guest's processes close their contexts on the way down. A hard reset (a kernel
  panic, a watchdog, `echo b > /proc/sysrq-trigger`) didn't: the next boot's contexts of the same
  ids failed (`Failed to create process resource for puid 1`), SurfaceFlinger never started and
  the boot never completed. Now a reset removes the guest's blob mappings at once and releases
  its resources and contexts when the device next activates, on the GPU worker before the
  guest's first command. Released at the reset instead, a shutdown destroyed contexts just
  before exit: the renderer's cleanup thread was then still destroying their Vulkan instances
  while `exit` ran its static destructors, and one stop in four ended in SIGSEGV. The deferred
  release costs nothing measurable (`-verbose` prints it): a hard reset of a 720×1280 phone
  leaves 50 resources and 14 contexts, released in under 1 ms, and after an `adb reboot` the
  guest has already given back everything (0 and 0). A reset first gives the fences the guest
  still waits on up to 100 ms to signal (none were pending in either kind of reset), and fences
  signalled after the device stopped reach no one. The renderer gets the device's fence ids, from
  a counter that never restarts, not the guest's, which start again at 1 each boot: a fence of
  the last boot that the renderer signals after the next one started (fence 4000 on context 1,
  ring 0) is known by its id and completes nothing, where with the guest's ids it would have
  completed the new boot's requests up to 4000 before their work was done. Between a reset and
  the next boot's first post, host screenshots and streams show black instead of the last boot's
  final frame; a Vulkan-only renderer's post notices are held until the renderer posts a buffer
  of the new boot (a color buffer's handle can be the guest's resource id, which every boot
  allocates alike, so creation of that id counts too), and a notice already under way when the
  hold is set finishes before the screen goes black. Before, once the new boot signalled fences,
  screenshots failed (`Failed to read from ColorBuffer:3, not found`) until its first post. An
  earlier measurement had `adb reboot` 0.3 s slower with this change; that was the benchmark: its
  all-black test (0.3 s a screenshot) ran on every poll of the black screen, and it timed
  `sys.boot_completed` only after the first frame. Numbers in
  [benchmarks](benchmarks.md#emulator-internals).

## Development

```bash
cargo c && cargo lint && cargo t    # check, clippy -D warnings, tests (no hypervisor needed)
make run AVD=<name>                 # boot with the console on stdout
cd desktop && cargo test            # headless Rune view tests
```
