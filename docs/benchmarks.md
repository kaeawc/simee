# Benchmarks

Every number the README and [comparison.md](comparison.md) quote comes from this page. Each row
says what was run, on what, and when. The rules for keeping it current are in
[AGENTS.md](../AGENTS.md#benchmarks-and-public-docs).

## Reproducing a number

Every benchmark below names the script that reproduces it, under `scripts/bench/`. Each one:

- takes `--help`, `--rounds N` (default 3; the tables report medians), `--quick` (one short round:
  a smoke test, not a number to publish) and `--out DIR`;
- creates the AVDs and simulators it uses and deletes them at the end (AVDs with
  `emu -delete-avd`, simulators by UDID); it never touches another device, never opens or quits
  Simulator.app, never runs `adb kill-server`, and stops only the processes it started;
- alternates simee and the stock tool from round to round where it compares them;
- records the host (model, CPU, macOS or Linux version, CPU count, memory, the 1-minute load
  average from `spectra load` when installed, else the kernel's) and simee's commit at the start
  and the end, writes every sample to `bench/results/<name>-<time>.json`, and prints the result
  as a Markdown table in the shape the sections below use.

A benchmark that creates an AVD says how (`AVD created fresh`): its first boot is a real first
boot, with `-no-golden` passed wherever `emu -create-avd` would otherwise start the AVD from a
settled golden userdata.

```bash
scripts/bench/run-all.sh --list                    # every benchmark, what it needs, its command
scripts/bench/run-all.sh --only android-boot       # one of them
scripts/bench/run-all.sh --kind host --quick       # the device-free ones, one short round each
```

Before/after rows compare two builds of simee: build the "before" commit into its own target
directory (`CARGO_TARGET_DIR=<dir> cargo build --release --bin simee --bin emu` in a checkout of that
commit; sign its `emu`) and pass it with `--baseline` (`--emu` for `android-reset.sh`); the
scripts then alternate the builds. Rows marked "no script" say why: their method isn't
recoverable from what was recorded, or they need something the repository doesn't hold.

| Benchmark | simee | Baseline (stock tool, unless it says simee) | Host, date | Reproduce |
| --- | --- | --- | --- | --- |
| [Android boot, API 36](#android-emu-against-the-sdk-emulator) | 9.5 s cold, 3.5 s warm, 2.09 GB | SDK emulator 22.9 s, 17.1 s, 3.78 GB | M3 Max, 2026-11-24 | `scripts/bench/android-boot.sh --snapshot` |
| [Android boot, API 29](#api-29-gles-over-the-goldfish-pipe) | 4.8 s warm | SDK emulator 6.4 s | M3 Max, 2026-11-26 | `scripts/bench/android-boot.sh --image …android-29/default…` |
| [First boots and idle CPU, API 29/30](#api-29-and-30-first-boots-and-idle-host-cpu) | 2.9% / 2.1% of a core idle | SDK emulator 5.4% / 3.7% | M3 Max, 2026-11-26 | `scripts/bench/android-idle.sh` |
| [Idle host CPU, the NAT](#idle-host-cpu) | 5.9% of a core | not measured | M3 Max, 2026-11-25 | `scripts/bench/android-net.sh --mode idle` |
| [NAT throughput](#idle-host-cpu) | 610 MB/s up, 2.1 GB/s down | not measured | M3 Max, 2026-11-25 | `scripts/bench/android-net.sh --mode throughput` |
| [Frame pacing](#frame-pacing) | 0% / 0.25% janky | SDK emulator 5.6% / 6.2% | M3 Max, 2026-11-25 | `scripts/bench/android-frames.sh --device tablet\|phone` |
| [Software mode on KVM, API 36 x86_64](#android-software-mode-on-kvm-githubs-x86_64-runner) | 28.8 s cold, 16.6 s warm, 2.27 GB | SDK emulator 37.3.10 30.3 s, 19.2 s, 3.00 GB | GitHub `ubuntu-latest` VM (4-vCPU Xeon 8573C), 2026-11-27 | `scripts/bench/android-boot.sh --image system-images/android-36/default/x86_64 --lcd 720x1280@320` |
| [Software mode, CPU model](#android-software-mode-linux-without-kvm) | zygote 49.4 s | | 4-vCPU Xeon VM, 2026-11-26 | `scripts/bench/software-mode.sh --arms "tcg:…"` |
| [Software mode, boot](#boot-to-sysboot_completed-with-emus-devices) | 168 s, 0.75 cores idle | SDK emulator `-accel off` 2302 s, 1.09 cores | 4-vCPU Xeon VM, 2026-11-26 | `scripts/bench/software-mode.sh --arms "emu stock"` |
| [Software mode, `emu -avd`](#software-mode-through-emu--avd) | 161 s, tap 60.8 ms, screenshot 36.9 ms | SDK emulator unusable | 4-vCPU Xeon VM, 2026-11-26 | `android-frames.sh`, `api-steps.sh --android` |
| [Software mode, software-lite profile](#software-mode-a-smaller-screen-and-30-hz) | 0.46 cores idle, 1.99 cores swiping, p90 frame 200 ms | default profile 0.68, 2.41, 350 ms | 4-vCPU Xeon VM, 2026-11-27 | `scripts/bench/software-profile.sh` |
| [Android farm](#android-farm) | 8 settled in 266 s | not measured | not recorded, before 2026-11-24 | `scripts/bench/android-farm.sh` |
| [Device bundles](#android-device-bundles) | 1225 MB, 7.2 s export, 6.7 s import | no stock tool | M3 Max, 2026-11-27 | `scripts/bench/android-bundle.sh` |
| [iOS boot](#ios-simee-against-simctl) | 15.5 s first, 2.3 s later | `simctl` 18.9 s, 2.3 s | M3 Max, 2026-11-24 | `scripts/bench/ios-boot.sh` |
| [iOS farm, default and slim](#ios-simee-against-simctl) | slim: 8 of 8 in 392 s, 1158 MB | simee default profile: 5 of 8, 2176 MB | M3 Max, 2026-11-24 | `scripts/bench/ios-farm.sh` |
| [iOS settle, slim profile](#ios-simee-against-simctl) | 4 clones in 43 s | simee before: 229 s | M3 Max, 2026-11-26/27 | `scripts/bench/ios-settle.sh`, `ios-farm.sh` |
| [iOS slim profile, 62 or 74 jobs](#ios-the-slim-profiles-jobs-62-against-74) | 20.1 s settled | simee, 62-job profile: 33.2 s | M3 Max, 2026-11-27 | `scripts/bench/ios-settle.sh --baseline …` |
| [Time per automation step](#ios-simee-against-simctl) | tap 63.4 ms, screenshot 24.5 ms | simee before: 137.5, 99.7 ms | M3 Max, 2026-11-26 | `scripts/bench/api-steps.sh` |
| [Screenshot as a command](#ios-simee-against-simctl) | 36 ms via `simee serve` | `simctl io screenshot` 601 ms | M3 Max, 2026-11-26 | `scripts/bench/ios-command.sh` |
| [Emulator internals](#emulator-internals) | before/after rows | | various | see the section |
| [`launch` with and without restart](#launch-with-and-without-restart) | 17–136 ms | | M3 Max, 2026-11-27 | `scripts/bench/ios-api.py --suite launch` |
| [Per-call device lookup](#per-call-device-lookup-on-a-crowded-host) | 0.66 / 0.04 ms | simee before: 1.54 / 0.34 ms | M3 Max, 2026-11-26 | `ios-api.py --suite lookup` |
| [Android device resolution](#android-device-resolution) | exact name 0.14 ms, `booted` 0.39 ms (kept); one-shot call 6.7 ms | simee before: 0.25, 2.02 ms; 21 ms | 4-vCPU Xeon VM (Linux), 2026-11-28 | `scripts/bench/android-resolve.py --baseline …` |
| [Introspection](#introspection-processes-connections-kill-network_state) | 2.5–3.5 ms | `ps`/`lsof`/`kill`: 126–191 ms | M3 Max, 2026-11-27 | `ios-api.py --suite introspection` |
| [iPad pointer click](#ipad-pointer-click) | 408 ms later click | simee before: 3641 ms | M3 Max, 2026-11-27 | `ios-api.py --suite pointer` (timings) |
| [iOS input defaults](#ios-input-defaults) | tap 12.6 ms, type 20 chars 121 ms | simee before: 64.5, 749 ms | M3 Max, 2026-11-27 | `ios-api.py --suite input` (timings) |
| [iOS app preferences](#ios-app-preferences) | read 0.11 ms | | M3 Max, 2026-11-27 | `ios-api.py --suite prefs` (reads) |
| [iOS memory tools](#ios-memory-tools) | `mem_vmmap` 0.60 s … `mem_leaks` 2.30 s | | M3 Max, 2026-11-27 | `ios-api.py --suite mem` |
| [Desktop: Metal against wgpu](#desktop-runes-native-metal-backend-against-wgpu) | | | M3 Max, 2026-11-27 | no script (below) |
| [MCP server](#mcp-server) | 1 crate | `rmcp`: 75 crates, 41 s build | Linux VM, 2026-11-26 | `scripts/bench/mcp-build.sh` |
| [MCP idle subscription](#mcp-idle-subscription) | 1 wake-up a second, 0 ms CPU in 10 s | simee before: 98.5 wake-ups a second, 40 ms | 4-vCPU Xeon VM, 2026-11-27 | `cargo test --release -p simee-mcp idle_subscription -- --ignored --nocapture` |
| [Build times](#build-times-local) | 74–77 CPU s | simee before: 131–136 | Apple silicon, 2026-11-26 | `scripts/bench/build-times.sh` |

## Android: emu against the SDK emulator

One API 36 `google_apis` arm64 AVD (4 vCPU, 2 GB) on an M3 Max (macOS 26.6.2), 2026-11-24, SDK
emulator 37.2.13.0. Medians of 3 boots of each kind; host load average 3–5 during the boots.
These runs predate emu's Vulkan-only renderer (2026-11-25), which shortened a phone's warm boot
and lowered its peak memory on another AVD ([internals](#emulator-internals)); this table is due
a rerun with it.

- **Cold:** a new AVD's first boot (no userdata yet: emu formats and encrypts `/data`).
- **Warm:** a later boot of the same AVD. The SDK emulator runs with `-no-snapshot-load`; emu
  has no snapshots, so both do a full boot.
- **Snapshot:** the SDK emulator restoring its quickboot snapshot, for reference.

```bash
scripts/bench/android-boot.sh --snapshot          # new AVD per round: cold and warm, both tools, stock's snapshot
scripts/android-bench.sh <label> <avd> emu|stock [-no-snapshot-load]   # one boot, one result line
```

| Measure | emu | SDK emulator | Ratio |
| --- | --- | --- | --- |
| Cold: launch until `sys.boot_completed` | 9.5 s | 22.9 s | 2.4× faster |
| Cold: launch until `adb` lists the device | 5.2 s | 14.9 s | 2.9× faster |
| Cold: peak RSS, process tree | 3764 MB | 4988 MB | 25% less |
| Warm: launch until `sys.boot_completed` | 3.5 s | 17.1 s | 4.9× faster |
| Warm: launch until `adb` lists the device | 1.6 s | 8.5 s | 5.3× faster |
| Warm: peak RSS, process tree | 2090 MB | 3780 MB | 45% less |
| Snapshot restore until `sys.boot_completed` | none | 2.8 s | |
| `adb shell echo`, warm | 0.02 s | 0.03 s | equal |
| Guest CPU, 100k shell loop, warm | 0.22 s | 0.36 s | |
| Guest CPU, 4× md5, warm | 0.43 s | 0.59 s | |

These replace the 2026-11-22 run, whose 1.67 GB peak memory for emu (against 5.09 GB) did not
hold up when remeasured.

### API 29 (GLES over the goldfish pipe)

One API 29 `default` arm64 AVD (2 GB, 720x1280), same host (Apple silicon Mac, 16 CPUs, macOS
26.6), 2026-11-26, SDK emulator 37.2.13.0. Warm boots (the AVD had booted before under both),
medians of 3, emu and stock alternating; host load average 21–30 during the runs.

```bash
scripts/bench/android-boot.sh --image system-images/android-29/default/arm64-v8a --lcd 720x1280@320
PORT=5610 scripts/android-bench.sh a29-emu-N <avd> emu        # the single boots it runs
PORT=5612 scripts/android-bench.sh a29-stock-N <avd> stock
```

| Measure | emu | SDK emulator | Ratio |
| --- | --- | --- | --- |
| Warm: launch until `sys.boot_completed` | 4.8 s | 6.4 s | 1.3× faster |
| Warm: launch until `adb` lists the device | 2.4 s | 3.5 s | 1.5× faster |
| Warm: peak RSS, process tree | 1596 MB | 1692 MB | 6% less |
| Guest CPU, 100k shell loop, warm | 0.29 s | 0.59 s | |
| Guest CPU, 4× md5, warm | 0.43 s | 0.94 s | |

First boots (`-wipe-data`) and warm boots of one API 29 `default` AVD under emu by host GLES
backend (#94), same host, 2026-11-26, rounds of first boot, warm boot, warm boot on SwiftShader,
3 rounds; host load average 10–14. Seconds from launch until `sys.boot_completed` and until the
launcher has focus (`dumpsys window`), polled once a second, so whole seconds. On Metal a first
boot never reaches the launcher (1 of 1 that day, 4 of 4 earlier: the renderer fault in
[android.md](android.md#performance-notes)); emu now runs a first boot on SwiftShader. Not yet
compared with stock's first boot.

```bash
scripts/bench/android-idle.sh --arms "emu emu:ANGLE_DEFAULT_PLATFORM=metal" --launcher --window 0          # first boots
scripts/bench/android-idle.sh --warm --arms "emu emu:ANGLE_DEFAULT_PLATFORM=swiftshader" --launcher --window 0  # warm
```

By hand, as these rows were run:

```bash
target/release/emu -avd <avd> -port 5620 -wipe-data   # first boot; without -wipe-data: warm
ANGLE_DEFAULT_PLATFORM=swiftshader target/release/emu -avd <avd> -port 5620   # warm, SwiftShader
# then once a second: adb -s emulator-5620 shell getprop sys.boot_completed, and
# adb -s emulator-5620 shell dumpsys window | grep mCurrentFocus (Launcher)
```

| Boot | Host GLES | `sys.boot_completed`, median (runs) | Launcher, median (runs) |
| --- | --- | --- | --- |
| First | ANGLE on SwiftShader (emu's default) | 4 s (4, 3, 4) | 4 s (4, 4, 5) |
| First | ANGLE on Metal | 9 s (1 run) | never (watchdog restarts) |
| Warm | ANGLE on Metal (emu's default) | 5 s (4, 5, 7) | 6 s (6, 5, 7) |
| Warm | ANGLE on SwiftShader | 4 s (4, 3, 4) | 4 s (4, 4, 5) |

### API 29 and 30: first boots and idle host CPU

One API 29 `default` and one API 30 `google_apis` arm64 AVD (4 vCPU, 2 GB, 720x1280), each booted
under emu built from `main` (1ea8508), emu with the reference RIL's modem answers (#83) and SDK
emulator 37.2.13.0, same host (Apple silicon Mac, 16 CPUs, macOS 26.6), 2026-11-26, 3 rounds
alternating the three (stock's third API 30 run waited 40 minutes for a boot slot and ran just
after midnight). Every boot is a first boot (`-wipe-data`), all with `-no-window -no-audio
-no-boot-anim -no-snapshot-save`; the boot time is launch until `sys.boot_completed` (polled every
0.5 s). Each device then sits 120 s, and idle host CPU is the emulator's process tree's CPU time
over the next 120 s (`ps -o time`). Other lanes' work put the 1-minute load between 5 and 48
during the boots and idle windows (one idle window ended at 109; its numbers are in line with the
others).

```bash
scripts/bench/android-idle.sh                                                     # API 29 default
scripts/bench/android-idle.sh --image system-images/android-30/google_apis/arm64-v8a   # API 30
# by hand, as run:
emu -avd <avd> -port 5640 -no-window -no-audio -no-boot-anim -no-snapshot-save -wipe-data
emulator -avd <avd> -port 5644 -no-window -no-audio -no-boot-anim -no-snapshot-save -wipe-data
# then adb -s emulator-<port> shell getprop sys.boot_completed every 0.5 s; 120 s later,
# ps -o time= -p <pid and children> at the start and end of 120 s
```

| Measure | emu | SDK emulator |
| --- | --- | --- |
| API 29 first boot until `sys.boot_completed` | 3.7 s (3.7, 3.2, 7.7) | 4.8 s (5.9, 4.8, 4.8) |
| API 30 first boot until `sys.boot_completed` | 18.0 s (31.3, 17.4, 18.0) | 17.0 s (20.5, 17.0, 16.3) |
| API 29 idle host CPU | 2.9% of a core (2.2, 2.9, 3.0) | 5.4% (4.1, 8.2, 5.4) |
| API 30 idle host CPU | 2.1% of a core (2.1, 2.5, 2.0) | 3.7% (3.5, 3.7, 3.8) |

The "before the modem fix" numbers below are a build before #148: pass it as an arm's emu
(`--arms` takes `emu` only for this checkout's build, so run the script from a checkout of that
commit).

Before the modem fix, emu from `main` idled at 149% of a core on API 29 (141.8, 149.3, 161.7)
and 156% on API 30 (156.3, 163.5, 156.1), with `com.android.phone` at 68% of a guest core and
`system_server` at 28% (`top`, API 29, run 1): by the end of each idle window the radio log
showed the phone switching between GSM and CDMA 1–10 times (none on stock or the fixed emu), and
the reference RIL had sent the modem 593 AT commands against stock's 159 and the fixed emu's 152
(API 30, run 2). Its first boots took 4.3 s (8.0, 4.3, 3.7) and 18.5 s (25.5, 18.5, 17.9).
The mechanism is in [android.md](android.md#performance-notes).

### Idle host CPU

The same API 36 AVD, warm-booted under emu built without and with its network device (#29),
alternating, 3 runs each, M3 Max, 2026-11-24. Each run waits 60 s after `sys.boot_completed`,
then takes emu's CPU time over 240 s (`ps -o time`) and counts `SETUP_DATA_CALL` in the radio log
over the same window. Other work on the host put the load average between 4 and 34 across the
runs.

| emu | Idle host CPU, median (runs) | Data calls in 240 s |
| --- | --- | --- |
| No network device | 65.9% of a core (61.6, 65.9, 66.3) | 276–294 |
| Link-only virtio-net | 6.0% of a core (5.7, 6.0, 6.6) | 0 |

Without an `eth0`, the guest's telephony retries the data call without a delay; with one, the
call succeeds once and the guest goes idle. No script: the "no network device" arm is a build
before #29 (emu has had a network device since), and these runs' command wasn't recorded beyond
the description above; `EMU_NET=link` (the link-only row) is what `scripts/net-bench.sh idle`
measures today.

With the user-mode NAT behind it (#36): one fresh API 36 google_apis AVD and one emu build, the
NAT on and off (`EMU_NET=link`), 3 warm boots each, alternating after one uncounted first boot,
M3 Max (16 CPUs), 2026-11-25. Each waits 60 s after `sys.boot_completed`, then takes emu's CPU
time over 60 s (`ps -o time`). `scripts/net-bench.sh idle <avd>`. Other lanes' work on the host
put the 1-minute load between 4.5 and 77 across the runs (each run's load at its start and end
below); the first link-only run had 65–77, the rest 4.5–15.6. Reproduce:
`scripts/bench/android-net.sh --mode idle`.

| emu | Idle host CPU, median (runs, 1-minute load) | Guest internet |
| --- | --- | --- |
| Link-only virtio-net | 5.6% of a core (7.5 at 77–65, 5.6 at 9.3–8.9, 4.9 at 5.4–4.5) | none |
| virtio-net with the NAT | 5.9% of a core (5.9 at 11.9–8.9, 5.0 at 9.6–7.4, 7.3 at 10.8–15.6) | validated |

The difference is inside the run-to-run spread. `sample <pid> 5` on the idle NAT device found its
net thread blocked in `poll` in all 3939 samples.

Throughput through the NAT on the same AVD, build and day, after the boot above: 200 MiB between
the guest's toybox `nc` and a Python socket server on the host's loopback (the guest's
10.0.2.2), one uncounted warm-up transfer then 3 runs each way, all timed on the host by the
server (from accept to the guest's last byte, or to the guest's close after the host's last
byte); the 1-minute load was 6.5–7.5. `scripts/net-bench.sh throughput <avd>`; with its own
AVD: `scripts/bench/android-net.sh --mode throughput`.

| Direction | Time for 200 MiB, median (runs) | Rate |
| --- | --- | --- |
| Guest to host | 0.342 s (0.321, 0.349, 0.342) | about 610 MB/s |
| Host to guest | 0.102 s (0.109, 0.102, 0.098) | about 2.1 GB/s |

The SDK emulator's slirp was not measured on this host.

### Frame pacing

How smoothly the guest draws under steady motion: 10 app-drawer swipes up and down on the
launcher, then the launcher's own frame stats (`dumpsys gfxinfo`). Janky frames missed their
deadline. One API 36 `google_apis` AVD of each size (4 vCPU, 2 GB) on the M3 Max, 2026-11-25,
SDK emulator 37.2.13.0, each warm-booted and given 60 s to settle, 3 runs of each alternating;
host load average 4–7. emu with GL is the same build with `EMU_RENDERER_GL=1`, as emu ran before
the Vulkan-only renderer. Host CPU is emu's process over the run; the stock emulator's runs in a
child process the script doesn't track.

```bash
scripts/bench/android-frames.sh --device tablet          # and --device phone: both tools, warm boots; 3 alternating runs each
scripts/android-jank.sh emulator-<port> [emulator pid]     # one run on a booted device, one key=value line
```

The "emu with GL" rows can't be rerun at this commit: emu no longer has `EMU_RENDERER_GL`, so they
need a build from before the Vulkan-only renderer (2026-11-25) as the emu arm.

| Device | Emulator | Janky frames, median (runs) | Frame time p90 / p99 | Host CPU |
| --- | --- | --- | --- | --- |
| Pixel Tablet, 2560×1600 | emu | 0% (0, 0.56, 0) | 5 / 6 ms | 49% of a core |
| | emu with GL | 9.5% (9.5, 9.9, 8.7) | 22 / 48 ms | 96% |
| | SDK emulator | 5.6% (5.9, 5.5, 5.6) | 18 / 23 ms | |
| Phone, 1080×2400 | emu | 0.25% (0.25, 0.13, 0.25) | 5 / 5 ms | 33% |
| | emu with GL | 5.0% (6.1, 5.0, 5.0) | 14 / 31 ms | 63% |
| | SDK emulator | 6.2% (8.4, 5.7, 6.2) | 18 / 30 ms | |

Both display at 60 Hz. The stock emulator's tablet rendered about 1350 frames over the swipes and
emu's about 530; emu with the guest on stock's GLES driver (`EMU_GUEST_EGL=emulation`) rendered
1254, so the count follows the guest's driver. On the phone both render about 790.

## Android: software mode on KVM (GitHub's x86_64 runner)

Software mode with QEMU on the host's KVM (`-accel kvm -cpu host`, chosen automatically when
`/dev/kvm` is usable) against the SDK emulator 37.3.10 on the same KVM, on a GitHub-hosted
`ubuntu-latest` runner: a VM with 4 vCPUs of a Xeon Platinum 8573C and 15.6 GB, Ubuntu 26.04
(kernel 7.0, Azure), 2026-11-27, simee 4605415. API 36 `default` x86_64 image, 720x1280@320, 4
vCPUs and 2 GB per AVD, a new AVD each round (`-no-golden`), the arms' order alternating, medians
of 3 rounds ([run 46070750188](https://github.com/kaeawc/simee/actions/runs/46070750188), the
`Software mode` workflow's kvm leg; a manual or main run repeats it):

```bash
scripts/bench/android-boot.sh --image system-images/android-36/default/x86_64 --lcd 720x1280@320
```

| Measure | emu | SDK emulator 37.3.10 |
| --- | --- | --- |
| cold: launch until sys.boot_completed, s | **28.8** (28.8, 29.3, 28.3) | 30.3 (30.7, 30.3, 28.9) |
| cold: launch until adb lists the device, s | **16.7** (16.7, 17.1, 16.7) | 19.2 (19.9, 19.2, 18.7) |
| cold: peak RSS of the process tree, MB | **2270** | 2999 |
| warm: launch until sys.boot_completed, s | **16.6** (16.6, 16.4, 16.7) | 19.2 (19.5, 19.2, 19.1) |
| warm: launch until adb lists the device, s | **6.8** (6.1, 6.8, 7.1) | 10.8 (10.8, 10.8, 10.7) |
| warm: peak RSS of the process tree, MB | **1858** | 1954 |
| warm: guest 4x md5 of 64 MiB, s | 0.35 | 0.52 |
| warm: guest 100k shell loop, s | 0.74 | 0.77 |
| warm: adb shell echo, s | 0.04 | 0.04 |

Three of the twelve boots (emu cold in round 3, both warm boots in round 2) saw a 1-minute load
just above the 4 CPUs at a sample (`starved=yes`); each is inside the other rounds' spread, and
without them the medians are emu 29.1 s cold and 16.7 s warm, the SDK emulator 30.3 s and 19.3 s.
Both use the renderer from the same emulator package. emu uses it on SwiftShader Vulkan. The SDK emulator's
default for `-no-window` on a host with no GPU is lavapipe Vulkan with SwANGLE GLES (`emuglConfig_init:
vulkan_mode_selected:lavapipe gles_mode_selected:swangle` in its log). `cold` includes each one
formatting its own userdata. The runner is a shared cloud
VM, so absolute times move between runs; the comparison is within one run.

## Android: software mode (Linux without KVM)

The CPU decision for hosts with no hardware virtualization
([#55](https://github.com/kaeawc/simee/issues/55)), measured on its motivating host: a
Firecracker x86_64 guest (Ubuntu 22.04, 4 vCPUs of a Xeon at 2.2 GHz with AVX-512, 3.8 GiB RAM,
no `/dev/kvm`, no GPU), 2026-11-26. One API 36 `default` image of each architecture (r2; the
google_apis x86_64 image's 4.2 GB `system.img` didn't fit this host's 10 GB disk beside the
toolchain), 4 vCPUs and 2 GB, booted by plain QEMU 11.1.3 TCG with stock's disk layout and no GPU,
so these time the translator alone, before any of emu's devices. Without a GPU SurfaceFlinger
can't start, so the milestones are a root shell on the console and zygote's first start
(`boot_progress_start`, in guest uptime). Then the framework is stopped and android-bench.sh's
guest CPU microbenchmarks run. Medians of 3 warm boots after one uncounted first boot; each boot's
microbenchmarks are the median of 3 rounds.

```bash
scripts/bench/software-mode.sh --qemu <qemu-system-x86_64> --arms "tcg:max,-erms,-fsrm,-fzrm,-fsrs,-fsrc tcg:max"
scripts/tcg-bench.py <sysdir> <workdir> --qemu <qemu-system-x86_64|aarch64> [--cpu MODEL] [--boots 3]
```

The wrapper runs x86_64 images only; the arm64 rows and the single-boot configurations
(`--tb-size`, `--threads`) run `scripts/tcg-bench.py` directly.

| Guest | CPU model | Shell | Zygote starts | 100k shell loop | 4× md5 of 64 MiB | QEMU peak RSS |
| --- | --- | --- | --- | --- | --- | --- |
| x86_64 | `max,-erms,-fsrm,-fzrm,-fsrs,-fsrc` | 16.5 s | 49.4 s | 9.8 s | 1.82 s | 1138 MB |
| x86_64 | `max` | 18.9 s | 54.5 s | 10.1 s | 2.75 s | 1126 MB |
| arm64 | `max,pauth=off,sve=off` | 13.8 s | 55.6 s | 17.3 s | 3.67 s | 1113 MB |

The same microbenchmarks under emu on Hypervisor.framework take 0.22 s and 0.43 s (M3 Max,
[above](#android-emu-against-the-sdk-emulator)). Single boots of other configurations on the same host and day, each with the median of 3
microbenchmark rounds (`--cpu`, `--tb-size`, `--threads`; the x86_64 ones with the guest's
`mitigations=off`, which made no difference, see below), which decided the defaults:

| Configuration (single boot) | Zygote starts | 100k shell loop | 4× md5 |
| --- | --- | --- | --- |
| x86_64, `max` without the fast-string features (the best row's model) | 50.1 s | 11.0 s | 1.98 s |
| x86_64, Haswell model (AVX2) without ERMS | 56.3 s | 10.7 s | 2.23 s |
| x86_64, Haswell model with ERMS | not recorded | 10.2 s | 3.03 s |
| x86_64, SSE4.2-only model (`qemu64,+ssse3,+sse4.1,+sse4.2,+popcnt,+cx16`) | 66.0 s | 10.4 s | 2.84 s |
| x86_64, Haswell without ERMS, translation cache 256 MiB instead of QEMU's default 1 GiB | 54.5 s | 13.9 s | 3.43 s |
| x86_64, Haswell without ERMS, one translator thread for all vCPUs (`thread=single`) | 115.0 s | 17.7 s | 11.7 s |
| arm64, `max,pauth=off,sve=off`, booted in the same sequence as the next three | 72.4 s | 19.3 s | 4.53 s |
| arm64, pointer authentication emulated (`pauth-impdef=on`) | 78.3 s | 19.9 s | 4.85 s |
| arm64, `max` (QARMA pointer authentication) | 77.1 s | 20.2 s | 4.55 s |
| arm64, `cortex-a76` | 97.0 s | 19.4 s | 6.33 s |

The arm64 single boots ran second to fifth after a first boot and reached zygote later than the
three counted boots above; compare them with each other.

What decided it:

- **x86_64 images on x86_64 hosts.** Same-ISA translation runs guest code 1.8 (shell loop) to
  2.0 (md5) times faster than the best arm64 configuration, and arm64 takes 13% longer to reach
  zygote, though it reaches the shell sooner (its kernel and first-stage init are lighter). What
  comes after zygote (the framework, ART) is guest code, so the guest-code ratio decides.
- **QEMU 11 or newer.** Ubuntu 22.04's QEMU 6.2 panics this guest within seconds under
  multi-threaded TCG (`int3` in `kmem_cache_alloc`: a CPU running the kernel's live-patching
  breakpoint after the patch finished); 11.1.3, which has QEMU's fix for it
  ([LP #2051965](https://bugs.launchpad.net/ubuntu/+source/qemu/+bug/2051965)), booted these
  20-odd boots without it. The window isn't closed: one of about a dozen full boots on emu's
  devices, before the boot rows below, hit the same panic.
- **Hide the fast-string features.** With ERMS/FSRM advertised the guest kernel's memcpy is
  `rep movsb`, which TCG runs as a byte loop: md5 of `/dev/zero` takes 51% longer and zygote
  starts 10% later.
- **Keep the 1 GiB translation cache and one translator thread per vCPU.** A single translator
  thread, the model a translator library such as Unicorn offers, takes twice as long to reach
  zygote (115 s against 56.3 s for the same CPU model) and five times as long for the parallel
  md5s, so option 2 of #55 is not pursued.
- `mitigations=off` in the guest made no difference: a single boot with it (zygote 50.1 s, md5
  1.98 s) falls inside the spread of an earlier set of 3 boots without it (48.8 to 50.1 s, 1.84 to
  2.05 s).

adb into the same guest under QEMU 11 TCG on the same host, through emu's virtio-vsock served
over vhost-user (`crates/emu-vmm/examples/vhost_user_vsock.rs`, emu's adb bridge on the host
side) and, for comparison on the same boot, through QEMU's own user-mode network to adbd's TCP
listener (`-netdev user,hostfwd`). Framework stopped; 21 `adb shell echo` round trips and 3
transfers of 64 MiB each way per round, two rounds alternating, every transfer byte-checked.

| Transport | `adb shell echo`, median (2 rounds) | 64 MiB push | 64 MiB pull |
| --- | --- | --- | --- |
| emu's vsock over vhost-user | 149, 131 ms | 42 to 54 MiB/s | 62 to 99 MiB/s |
| QEMU's user network to adbd's TCP port | 134, 125 ms | 38 to 45 MiB/s | 13 to 18 MiB/s |
| emu's vsock, every queue called on each raise (before) | 157, 171 ms | 38 to 48 MiB/s | 47 to 66 MiB/s |

No script for this table: the guest was wired to `crates/emu-vmm/examples/vhost_user_vsock.rs`
by hand on a QEMU command line that wasn't recorded.

Calling every queue on each raise cost the guest a spurious interrupt per completion, because
vsock's queues share one MSI vector: 14 vsock interrupts per round trip before, 9 after
(`/proc/interrupts`, 20 round trips). emu's vsock server used 0.53 s of CPU for a whole boot plus
the 384 MiB of transfers, 21 MB peak RSS: what's left per round trip is the guest under
translation. The SDK emulator's own adb path has no row: it doesn't start on this host (below).

The SDK emulator (37.3.10) does not start on this host as first set up: it requires 7.37 GB of
free disk to create userdata whatever the AVD's data size (2.2 GB was free), and warns that
3939 MiB is under its suggested 4096 MiB of RAM. Its `-accel-check` reports `/dev/kvm is not
found`. With the host grown to 8 GiB and a 20 GB disk it starts, and the boot rows below compare
against it.

The renderer on the same host, no VM: the SDK's Linux `libgfxstream_backend.so` (37.3.10, from
`emu -provision-renderer`) on lavapipe, initialized Vulkan-only as emu runs it, then a 720×1280
RGBA color buffer uploaded and read back the way frames are (median of 9 reads per run), 5 runs
each. Before this, `-provision-renderer` fetched Apple silicon's archive on every host, so Linux
had no renderer it could load.

```bash
scripts/bench/host-paths.sh --only renderer --lib64 <lib64>     # on the Linux host
cargo run --release -p simee-emu-gfxstream --example renderer_caps -- -vulkan-only <lib64>
```

| Vulkan driver | Renderer init, median (runs) | 720×1280 readback, median of the runs' medians |
| --- | --- | --- |
| The SDK's lavapipe (`lib64/vulkan`) | 108 ms (108, 112, 140, 104, 91) | 1.26 ms |
| The host's Mesa lavapipe (Ubuntu 22.04, LLVM 15) | 143 ms (105, 155, 143, 140, 146) | 1.21 ms |

emu uses the SDK's: the two are equal here, and the SDK's is the same on every host. Rendering
throughput, where they could differ, needs a booted guest. Booted, the SDK's lavapipe crashed in a
render thread on every boot, so boots render on its SwiftShader (below).

### Boot to `sys.boot_completed` with emu's devices

The same host, grown to 8 GiB of RAM and a 20 GB disk, 2026-11-26. The API 36 `default` x86_64
image, 4 vCPUs and 2 GB, under QEMU 11.1.3 TCG with emu's devices
(`crates/emu-vmm/examples/soft_devices.rs`: vsock, console, net and the GPU over vhost-user, the
goldfish address space and pipe over vfio-user). The renderer is the SDK's gfxstream on its
SwiftShader. Five warm boots after one uncounted first boot. Each boot's time runs from QEMU's
start to `getprop sys.boot_completed` reading 1 on the console. Host CPU is QEMU's plus emu's,
over 60–120 s after boot complete (the window the Mac rows use) and over 240–300 s. Peak RSS is
each process's `VmHWM`.

```bash
scripts/bench/software-mode.sh --qemu <qemu-system-x86_64> --arms "emu stock" --rounds 5
scripts/tcg-bench.py <sysdir> <workdir> --qemu <qemu-system-x86_64> --emu-devices target/release/examples/soft_devices \
  --lib64 <renderer lib64> --icd swiftshader --boots 5
```

| Run | Failed boots | Boot to `sys.boot_completed`, median (runs) | Host CPU 60–120 s after | 240–300 s after | Peak RSS, QEMU + emu |
| --- | --- | --- | --- | --- | --- |
| emu's devices | 0 of 5 | 168 s (165.6, 150.5, 168.0, 183.7, 181.0) | 2.14 cores | 0.75 cores | 3058 + 233 MB |
| Before: goldfish writes posted | 2 of 5 (a guest panic, a hang) | 170 s (180.9, 170.0, 160.8) | 3.94 cores | not measured | 2992 + 173 MB |

The first boot of a fresh userdata took 375.5 s. With QEMU's default posted writes on the
goldfish devices, one boot panicked (`zap_huge_pmd`) and one never completed (SurfaceFlinger
crashed in `init`, zygote restarted). `x-no-posted-writes` fixed both (docs/android.md, "Software
mode"). The "before" CPU column was measured from 60 s after boot complete only; most of that
load is the guest's post-boot work, and host CPU settles at about a third of it by 240 s.
On one interactive boot, about 4 minutes after boot complete, `top -H` showed QEMU's four vCPU
threads at about 16% of a core each, its main loop at 7%, and emu under 1%. The guest's own
`top` showed it 84% idle; the rest was system_server, telephony and logd.

Under emulation, apps can exceed Android's startup timeouts. One interactive boot showed "System
UI isn't responding" after com.android.phone overran its startup (its main thread runnable, not
blocked).

The SDK emulator's software mode on the same host and day: 37.3.10 with the same image in an AVD
of the same shape (4 vCPUs; stock raised its 2 GB to 2560 MB). It runs with `-accel off`, which
it refuses unless `ANDROID_I_WANT_MY_TCG=yes` is set, plus SwiftShader and `-no-snapshot`. One
uncounted first boot, then three, each with a 40-minute budget.

```bash
scripts/tcg-bench.py <sysdir> <workdir> --stock <sdk>/emulator/emulator --avd <x86_64 avd> --boots 3 --budget 2400
```

| Run | Boot to `sys.boot_completed`, median of 3 (runs) | Host CPU 60–120 s after | 240–300 s after | Peak RSS |
| --- | --- | --- | --- | --- |
| emu's devices (above) | 168 s | 2.14 cores | 0.75 cores | 3058 + 233 MB (3.29 GB) |
| The SDK emulator, `-accel off` | 2302 s (over 2400, 1603.3, 2301.9) | 1.08 cores | 1.09 cores | 3116 MB (3.12 GB) |

The SDK emulator's software mode runs every vCPU on one translator thread (`top -H`: one QEMU thread
at 93% of a core, the rest near idle). Its first boot and first counted boot didn't reach
`sys.boot_completed` in 40 minutes. In the guest, `system_server` was killed by its watchdog 4
times, and zygote started 5 times in the first. The two that completed took 27 and 38 minutes,
13.7 times emu's median. Stock idles at about one core because its single translator thread
never goes idle. In this mode it also gives the guest GLES over the pipe (`gltransport=pipe`,
`skiagl`) rather than ANGLE. Builds of emu ran at the lowest priority on 2 of the 4 cores during
some of the counted boots.

### Software mode through `emu -avd`

`emu -avd` booting the same AVD itself (#106), same host and day, three rounds. Each round is a
warm boot, a 60 s settle, three runs of `scripts/android-jank.sh emulator-5554 <emu pid>` (10
launcher swipes up and down, then the launcher's `dumpsys gfxinfo`; host CPU covers emu and its
QEMU), and `scripts/ios-step-bench.py <socket> x86_36 20 360 600` through `simee serve` (20 calls
of each step on an empty spot of the workspace, after a warm-up). Boot times are emu's own `boot
completed` line. Medians of the 3 boots, the 9 jank runs, and the 3 step runs' medians.
Reproduce on the same kind of host: `scripts/bench/android-frames.sh --image
system-images/android-36/default/x86_64 --arms "emu stock" --runs 3` and `scripts/bench/api-steps.sh
--android --image system-images/android-36/default/x86_64`.

| Measure | emu, software mode | The SDK emulator, `-accel off` (one boot) |
| --- | --- | --- |
| Warm boot to `sys.boot_completed` | 161.0 s (156.2, 165.5, 161.0) | 850 s, then unusable (below) |
| Launcher swipes: frame time p50 / p90 / p99 | 150 / 300 / 400 ms | not measurable |
| Launcher swipes: janky frames | 96.7% | not measurable |
| Host CPU while swiping | 2.46 cores | 1.06 cores |
| `tap` (holds 60 ms) | 60.8 ms | no display socket (adb only) |
| `screenshot` to a file / as base64 | 36.9 / 61.1 ms | no display socket |
| `tap`, then `screenshot` to a file | 101.4 ms | no display socket |

The stock emulator's boot set `sys.boot_completed` after 850 s. In the minutes after it, the guest
had no `package`, `window`, `input` or `gfxinfo` service (`cmd: Can't find service`), so every
swipe and frame query failed. It then crashed with SIGSEGV when told `emu kill`. simee's step
bench drives input and frames over emu's display socket, which the stock emulator doesn't have.

The launcher draws at about 7 frames a second in software mode: every frame's guest-side work runs
translated, and SwiftShader renders it on the same 4 cores. Taps and screenshots don't depend on
the guest's speed. A tap is the API's 60 ms hold plus under 1 ms, and a screenshot reads the last
frame emu already holds.

`kfence.sample_interval=0` (#106, the `int3` panics) against the image's KFENCE sampling (every
500 ms), through `scripts/tcg-bench.py --emu-devices ... --append`, four alternating rounds, one
boot of each per round (`scripts/bench/software-mode.sh --arms
"emu:--append,kfence.sample_interval=0 emu:--append,kfence.sample_interval=500" --rounds 4`):

| Arm | Boot, median (runs) | Host CPU 60–120 s after | 240–300 s after | Panics |
| --- | --- | --- | --- | --- |
| KFENCE sampling off | 165.4 s (175.9, 160.7, 161.1, 169.6) | 1.82 cores | 0.73 cores | 0 of 4 |
| KFENCE every 500 ms | 167.3 s (167.3, 164.3, 184.6, 167.2) | 1.67 cores | 0.74 cores | 0 of 4 |

Turning sampling off costs nothing measurable. The fix rests on the mechanism, not on a panic
rate, which these 8 boots are too few to show. A warm `emu -avd` boot had panicked with `int3`
in `__kmem_cache_alloc_node` 101 s in, on three CPUs at once. Lowest-priority builds of emu ran
on 2 cores during the 184.6 s run.

The guest display's refresh rate, 30 Hz against 60 (`androidboot.qemu.vsync`, which emu takes
from the AVD's `hw.lcd.vsync`), through `scripts/tcg-bench.py --emu-devices ... --vsync N`, three
alternating rounds, one boot of each per round (`scripts/bench/software-mode.sh --arms
"emu:--vsync,30 emu:--vsync,60"`). The ranchu composer marks its present fences
unreliable, so SurfaceFlinger keeps hardware vsync on even when nothing draws. On an idle
software-mode guest, Perfetto showed 60 composer callbacks a second, each a binder call that
SurfaceFlinger handles: 3.2 s of guest CPU per 14.5 s.

| Refresh rate | Boot, median (runs) | Host CPU 60–120 s after | 240–300 s after |
| --- | --- | --- | --- |
| 30 Hz | 169.0 s (154.4, 175.8, 169.0) | 1.94 cores | 0.61 cores (0.61, 0.69, 0.61) |
| 60 Hz (the default) | 175.7 s (169.2, 175.7, 177.6) | 2.12 cores | 0.73 cores (0.72, 0.74, 0.73) |

30 Hz settles 0.12 cores lower, and it was lower in every round. Software mode draws about 7
frames a second, so a 30 Hz display costs no frames. But apps see a 30 Hz display, so 60 stays the
default, and an AVD opts in with `hw.lcd.vsync=30`.

On GitHub's `ubuntu-latest` (4 vCPUs, `.github/workflows/software-mode.yml`), the first boot of
a fresh userdata reached `sys.boot_completed` in 243.9 s (one run). This host's first boot took
381.4 s. The CI number is that workflow's own run; it has no other script.

Where software mode spends its time, phase by phase, with its own reproduction steps:
[research/software-mode-profile.md](research/software-mode-profile.md) (a profile on a
Firecracker VM, 2026-11-27, not a comparison).

### Software mode: a smaller screen and 30 Hz

The `software-lite` profile (`emu -create-avd NAME -sysdir … -profile software-lite`: 540x960 at
240 dpi and `hw.lcd.vsync=30`) against the default 720x1280 at 320 dpi and 60 Hz, and each half
alone, on one AVD whose `config.ini` the script rewrites between boots (the same userdata, animations
on). A 4-vCPU Intel Xeon 2.2 GHz Firecracker VM with 3.8 GiB of RAM (3.3 GiB cgroup limit), no
`/dev/kvm`, 2026-11-27; QEMU 11.1.3, emu from main at 0b99d0b, API 36 `default` x86_64 r2, the
AVD 4 vCPUs and 1.5 GB. Three rounds, each booting the four arms warm in turn (the order alternating
between rounds), after one uncounted first boot: 12 boots, medians with the three runs in
parentheses. Idle is emu's and QEMU's CPU time over the 60 s that start 240 s after
`sys.boot_completed`; the swipes are `scripts/android-jank.sh` (10 up and 10 down launcher
swipes over adb, about 46 s), twice per boot, the first run being the coldest; frames per second
is the launcher's frames over the run's wall time; memory is the emu and QEMU processes'
proportional set size after the swipes (shared guest RAM counted once) and the peak sum of their
resident sizes sampled during the boot. Host load (1-minute average at the end of the idle
windows) was 0.4 to 1.1: the guest's own, with nothing else running.

```bash
scripts/bench/software-profile.sh --qemu <qemu-system-x86_64> --rounds 3
```

| Measure | default | 540x960 only | 30 Hz only | software-lite (both) |
| --- | --- | --- | --- | --- |
| Warm boot to `sys.boot_completed` | 174.0 s (152.6, 174.0, 188.4) | 171.1 s (165.1, 171.1, 191.0) | 172.6 s (172.6, 158.7, 194.9) | 165.0 s (164.7, 165.0, 171.7) |
| Warm boot to the launcher | 217.5 s (199.5, 217.5, 231.9) | 206.9 s (202.8, 206.9, 250.8) | 217.9 s (217.9, 195.9, 251.4) | 206.6 s (201.0, 206.6, 212.8) |
| Idle host CPU | 0.68 cores (0.56, 0.70, 0.68) | 0.57 (0.57, 0.56, 0.70) | 0.53 (0.53, 0.50, 0.54) | 0.46 (0.45, 0.46, 0.48) |
| Swipes, second run: frames in 20 swipes | 188 (188, 193, 179) | 186 (182, 196, 186) | 182 (182, 187, 178) | 193 (199, 193, 193) |
| Swipes, second run: frames per second | 4.08 (4.22, 4.08, 3.71) | 4.16 (4.16, 4.38, 3.92) | 3.97 (3.97, 4.25, 3.80) | 4.31 (4.56, 4.31, 4.20) |
| Swipes, second run: frame time p50 / p90 | 150 / 350 ms | 150 / 200 ms | 150 / 250 ms | 129 / 200 ms |
| Swipes, second run: janky frames | 97.3% (97.3, 96.9, 98.3) | 97.3% (94.0, 98.0, 97.3) | 87.2% (83.5, 87.2, 89.9) | 74.6% (71.4, 74.6, 80.8) |
| Swipes, second run: host CPU | 2.41 cores (2.31, 2.47, 2.41) | 2.09 (2.04, 2.09, 2.24) | 2.18 (2.18, 2.12, 2.27) | 1.99 (1.96, 1.99, 2.04) |
| Swipes, first run: frames in 20 swipes | 108 (114, 104, 108) | 118 (129, 108, 118) | 115 (115, 128, 103) | 127 (119, 134, 127) |
| Swipes, first run: frames per second | 2.26 (2.51, 2.24, 2.26) | 2.54 (2.88, 2.42, 2.54) | 2.49 (2.49, 2.85, 2.25) | 2.83 (2.83, 3.07, 2.75) |
| Swipes, first run: frame time p50 / p90 | 150 / 350 ms | 150 / 250 ms | 150 / 300 ms | 117 / 200 ms |
| Swipes, first run: janky frames | 96.5% (96.5, 94.2, 98.2) | 95.8% (95.4, 96.3, 95.8) | 80.9% (80.9, 78.9, 84.5) | 71.4% (71.4, 70.2, 78.0) |
| Swipes, first run: host CPU | 3.06 cores (2.97, 3.33, 3.06) | 2.93 (2.91, 2.93, 3.35) | 3.04 (3.04, 2.93, 3.21) | 2.97 (2.70, 2.97, 3.07) |
| Memory, PSS after the swipes | 2356 MB (2298, 2356, 2362) | 2321 (2349, 2314, 2321) | 2344 (2335, 2344, 2357) | 2309 (2337, 2309, 2303) |
| Memory, peak resident sum in the boot | 2197 MB (2195, 2197, 2219) | 2191 (2208, 2191, 2153) | 2194 (2207, 2194, 2186) | 2183 (2204, 2183, 2170) |

The profile settles 0.22 cores lower (a third) and was lower in every round (0.45, 0.46, 0.48
against the default's 0.56, 0.70, 0.68). The two halves nearly add: 0.11 from the screen and 0.15
from 30 Hz alone, 0.22 together. While swiping it costs 17% less CPU (2.41 against 1.99 cores) and
draws 3% more frames on a warm launcher (6% more a second), 18% more on the cold first run, with
the p90 frame time at 200 ms against 350. Most of the janky-frame drop is the 30 Hz deadline, 33
ms instead of 17: that arm alone is 87%, so the frame times are the number to read. Boot is not
a result: 165 s against 174 s is inside the 152.6 to 188.4 s spread of the default's own three
boots, and memory is the same to 2%, since the guest's 1.5 GB of RAM and QEMU's translation cache
dominate it. These are the numbers of one 4-vCPU host, and the default's idle cost varied from 0.56
to 0.70 across three boots of the same AVD: the screen's 0.11 on its own is inside that spread,
while the profile's 0.45 to 0.48 is under every default run.

## Android farm

Clones of one settled golden AVD booted together, each until it is settled (boot complete and
idle), same host and day. `scripts/farm-bench-android.sh`. The host and date weren't recorded
with this table; it predates emu's network device (#29, 2026-11-24). Reproduce, with a golden it
creates and deletes: `scripts/bench/android-farm.sh --sizes "1 4 8"`. More farm numbers, with
their commands: [farm-android.md](farm-android.md).

| Devices | All settled | Failures | Host memory used per device |
| --- | --- | --- | --- |
| 1 | 72 s | 0 | 3.1 GB |
| 4 | 223 s | 0 | 2.8 GB |
| 8 | 266 s | 0 | 2.8 GB |

Most of the settle time is the guest's telephony retrying mobile data, since emu has no network
device yet ([#29](https://github.com/kaeawc/simee/issues/29)).

## Android: device bundles

`simee export` / `simee import` ([#170](https://github.com/kaeawc/simee/issues/170)) on an API 36
`google_apis` arm64 AVD (revision 7, 4 GB data partition) after its first boot under emu, 100 s of
uptime and a file written to `/data/local/tmp`; M3 Max, macOS 26.6.2, 2026-11-27, host load 5–50.
Chunks are LZ4 (`lz4_flex`, pure Rust). The zstd column is the same recipe on another fresh AVD
the same day, with zstd's C library at level 3 (level 19 gave 1193 MB, no smaller, in 16.4 s).
One run each. No stock tool exports an AVD, so there is no comparison with one.

```bash
scripts/bench/android-bundle.sh --verify   # new AVD per round: boot 100 s, export, import, boot the import
simee export <avd> <dir-or-file.tar>
simee import <file>.tar <new-name>       # then boot it: the file and settings are there
```

The zstd column was a build with zstd's C library, not shipped; it has no script. The two-clone
chunk sharing rows were measured by hand on two clones' exports.

| Measure | LZ4 (shipped) | zstd 3 |
| --- | --- | --- |
| Disks' logical size (userdata, cache, encryptionkey) | 4383 MB | 4383 MB |
| Disks' allocated size on the host | 1243 MB | 1219 MB |
| Bundle (tar) | 1225 MB (98.5% of allocated) | 1192 MB (97.8%) |
| userdata in stored 1 MiB chunks / compressed | 1202 MiB / 1166 MiB (97.0%) | 1178 MiB / 1137 MiB (96.5%) |
| Export (8 threads), wall | 7.2 s | 7.6 s |
| Import from the tar (streamed, every hash checked), wall | 6.7 s, 15 MB peak RSS | 7.0 s, 18 MB peak RSS |
| `/data` used, as the guest's `df` reports it | | 861 MB |
| Two clones of one golden, each booted once for 100 s: chunks shared | | 981 of ~1210 (81%) |
| The second clone's new chunks, given the first's, compressed | | 200 MiB |

User data is ciphertext (`/data` sits on dm-default-key, keyed from `encryptionkey.img`), so its
chunks don't compress: either codec's gain is the zero 4 KiB blocks inside 1 MiB chunks, and LZ4
gives up half a point for no zstd dependency. Hashing (BLAKE3 of the disks and of the 4.4 GB system
image) dominates both directions, so the codec doesn't move the times. The bundle stores ~1.14 GB
of non-zero user data for 861 MB of files, which suggests freed blocks that were never discarded
still hold ciphertext. Between clones of one golden, a boot dirties about 200 MiB of chunks.
Shrinking bundles (deltas against the golden, discarding freed blocks before export) is
[#171](https://github.com/kaeawc/simee/issues/171). An import leaves zero ranges as holes, but
APFS fills a hole shorter than ~16 MiB between written blocks with zeros (measured), so an
imported userdata takes ~4% more host space than its source (1260 MB against 1228 MB).

## iOS: simee against `simctl`

One new iPhone per run under each tool, same host, 2026-11-24, 3 runs each; the
boot returns once the device has finished booting (`simctl bootstatus` for `simctl`). Load on
this host varied during these runs, so treat them as a rough comparison. Reproduce:
`scripts/bench/ios-boot.sh`.

| Measure | simee | `simctl` |
| --- | --- | --- |
| First boot of a new device, median | 15.5 s | 18.9 s |
| Later boot, median | 2.3 s | 2.3 s |

`scripts/farm-bench-ios.sh`, 8 clones booted 4 at a time, same day (`scripts/bench/ios-farm.sh
--n 8 --k 4`, which makes and deletes the template):

| Profile | Settled | Footprint per device | Processes per device |
| --- | --- | --- | --- |
| default | 5 of 8 within 600 s | 2176 MB | 235 |
| `slim` | 8 of 8 in 392 s | 1158 MB | 148 |

Time to usable devices: fresh clones of one booted-once iPhone 17 (iOS 27.2) template, first boot
through `simee boot-many <clones> -j K --profile slim --json`, 2026-11-26, M3 Max, 16 CPUs, macOS
26.6, 3 interleaved runs of each with other work on the host (1-minute load 25-550: read the
ranges). "Before" is the same build with the slim disables applied after the boot and settle
judged on all CPU (`--settle-cpu all`), as until this change; the mechanism is in
[farm.md](farm.md#the-slim-profile). Reproduce: `scripts/bench/ios-farm.sh --n 4 --k 4 --profiles
slim` with this checkout and with the build before #60 (run from its checkout); one clone at a
time: `scripts/bench/ios-settle.sh --baseline <before build> --settle-cpu all`.

| Measure | before | after |
| --- | --- | --- |
| 1 clone: booted until settled, median (runs) | 95.0 s (97.2, 95.0, 52.7) | 33.3 s (44.2, 33.3, 28.3) |
| 4 clones at once (K=4): all settled, median (runs) | 229 s (229, 170, 238) | 43 s (35, 43, 71) |
| 4 clones at once: CPU-s each device used until settled, median (not like for like: "before" waited on all CPU, "after" on foreground CPU only, so its wait ends earlier and background work keeps running after it; docs/farm.md) | 167 | 76 |
| The slim profile's own time in that, 4 at once, median of 12 | 42.9 s | 5.4 s |

The slim profile with the disables also passed as the boot's `disabled_jobs` and read back from
launchd's own overrides file instead of `launchctl` ([farm.md](farm.md#the-slim-profile)): one
fresh clone at a time of a booted-once template, `simee boot <clone> --profile slim --settle`,
2026-11-27, M3 Max, 16 CPUs, macOS 26.6, CoreSimulator 1174.21, median of 3 interleaved runs,
1-minute load 11-29. "Before" is origin/main (690cd99) built the same way. Reproduce:
`scripts/bench/ios-settle.sh --baseline <690cd99 build>` (and `--runtime
com.apple.CoreSimulator.SimRuntime.iOS-18-0 --device-type "iPhone 16 Pro"`).

| Measure | before | after |
| --- | --- | --- |
| iPhone 17 Pro, iOS 27.2: the slim profile's own time (runs) | 1.2 s (1.1, 1.3, 1.2) | 0.0 s (0.0, 0.0, 0.0) |
| iPhone 17 Pro, iOS 27.2: booted until settled (runs) | 21.2 s (20.2, 22.4, 21.2) | 22.6 s (22.6, 21.1, 31.3) |
| iPhone 16 Pro, iOS 18.0: the slim profile's own time (runs) | 13.7 s (14.1, 13.7, 13.6) | 0.0 s (0.0, 0.0, 0.0) |
| iPhone 16 Pro, iOS 18.0: booted until settled (runs) | 27.5 s (28.0, 27.5, 27.3) | 22.6 s (20.9, 22.7, 22.6) |
| iPhone 16 Pro, iOS 18.0: CPU-s until settled, processes at settle | 47.8, 103-107 | 38.6, 95 |

On iOS 27.2 the 1.2 s was 5 `launchctl` spawns (the preset already kept every job off), and one
device's settle time doesn't show it: the runs spread more than that. On iOS 18.0, whose launchd
ignores the preset's file, "before" ran ~127 spawns to disable and stop all 62 jobs after they
had started, and left one running in 2 of 3 runs; "after" they never start.

Time per automation step on one booted iPhone 17 (iOS 27.2) through `simee serve`, one persistent
connection, `python3 scripts/ios-step-bench.py <socket> <udid> 20` (medians of 20 calls; the
median of 3 interleaved runs), 2026-11-26, same host, 48 AVDs and 100 simulators on it, 1-minute
load 21-32. "Before" routed every call by listing both platforms' devices. Reproduce:
`scripts/bench/api-steps.sh --baseline <before build>` (it makes its own device; the published
host also held 48 AVDs and 100 simulators):

| Measure | before | after |
| --- | --- | --- |
| tap (holds 60 ms) | 137.5 ms | 63.4 ms |
| screenshot, PNG written to a file | 99.7 ms | 24.5 ms |
| screenshot, base64 in the reply | 107.8 ms | 34.8 ms |
| tap, then screenshot to a file | 241.2 ms | 88.9 ms |

One screenshot as a command (a process per call), same device and hour, median of 7 interleaved
(load 13-69): `simee screenshot` forwarded to `simee serve` 36 ms, `simee screenshot` in its own
process (`SIMEE_IN_PROCESS=1`) 173 ms, `xcrun simctl io <udid> screenshot` 601 ms. Reproduce:
`scripts/bench/ios-command.sh --rounds 7 --calls 1`, which also times `simee tap` both ways
([farm.md](farm.md#forwarding-to-simee-serve)'s 321 against 83 ms).

## Emulator internals

These explain the table above. Each is a before and after of emu itself, with the mechanism
in [android.md](android.md#performance-notes). Reproducing them:

- Device-free rows (the screenshot's convert and encode, the frame store, vsock and adb
  throughput, the renderer's warm init and readback): `scripts/bench/host-paths.sh` runs them on
  this host. The vsock, adb and display-stream rows were measured on a 4-core Linux VM; a Mac
  gives other absolute numbers.
- Interrupt kicks (`EMU_VCPU_WAKE`): `scripts/bench/android-boot.sh --arms
  "emu:EMU_VCPU_WAKE=targeted emu:EMU_VCPU_WAKE=none"` (its cold rows are the first boots;
  `vcpu0_runs` and `cpu_s` are in android-bench.sh's per-boot lines in `bench/`).
- Resets: `scripts/bench/android-reset.sh --baseline <before emu>`.
- Launcher swipes after a change: `scripts/bench/android-frames.sh --arms emu` with each build's
  checkout.
- Metal's shader cache cold: no script. A cold run needs the bundle's Metal cache removed (or a
  new bundle id per run), which a script shouldn't do to the host's caches unasked.
- The Vulkan-only renderer rows need `EMU_RENDERER_GL`, which emu no longer has.
- No recorded method: first-boot `/data` encryption with kicks (22 to 8 s), `fsync` and APEX
  decompression (270 to 70 s) and whole-disk FITRIM (90 ms), all "an M-series Mac" on an earlier
  macOS; and the display stream's CPU per frame (12.8 / 7.1 to 2.0 / 0 ms). They stay as
  history; rerun them before quoting them.

| Change | Before | After | Measured on |
| --- | --- | --- | --- |
| Kick the target vCPU on an interrupt: first-boot `/data` encryption | 22 s | 8 s | M-series Mac, an earlier macOS |
| No kicks on an interrupt (`EMU_VCPU_WAKE` default `targeted` → `none` on macOS 26 and later): first boot (`-wipe-data`) of a 720×1280 API 36 phone, median (runs). A tie: the arms differ by less than the run-to-run spread | 11.4 s (11.4, 11.5, 11.4) | 11.6 s (11.8, 11.4; a third low-load run is owed) | M3 Max, macOS 26.6.2, 2026-11-26, runs at 1-minute load 7.7 to 8.1 (16 CPUs), arms interleaved, `EMU_VCPU_WAKE=targeted\|none scripts/android-bench.sh <label> <avd> emu -wipe-data` |
| Same runs, vCPU0 guest entries / emu CPU-seconds to `sys.boot_completed`, first boot, median | 348k / 21.7 | 257k / 22.1 | same runs (`vcpu0_runs`, `cpu_s`) |
| Same change, warm boot, median of 6 | 4.2 s (3.5 to 4.7) | 3.7 s (3.7 to 5.2) | same AVD and hour, host load 9.6 to 13.2 |
| Same change, vCPU0 guest entries to `sys.boot_completed`, first / warm boot, median | 338k / 59.7k | 249k / 41.4k | same runs (`vcpu0_runs`) |
| Same change, emu CPU-seconds to `sys.boot_completed`, first / warm boot, median | 25.0 / 9.3 | 23.4 / 8.7 | same runs (`cpu_s`) |
| Same change, launcher swipes a minute after boot: janky frames, median of 6 (2 boots × 3 runs) / emu CPU | 0.40% / 28% of a core | 0.20% / 25% | same AVD, host load 5 to 11, `scripts/android-jank.sh emulator-5600 <pid>` |
| Renderer init with Metal's shader cache cold / warm (mostly ANGLE's Metal shader library) | 4.5 and 5.1 s (2 of 20 launches) | 0.33 s (median of the other 18) | M3 Max, 2026-11-26, `emu -verbose` (`renderer initialized in`) |
| Same init without a guest, as an API 36 boot makes it: cold (an empty cache) / warm, median of 3 | 4.36 s (4.31, 4.36, 4.39) | 0.08 s (70 to 96 ms, 6 runs) | M3 Max, 2026-11-26, host load 23 to 27, `cargo run --release -p simee-emu-gfxstream --example renderer_caps -- -emu <lib64>`, each cold run under a new bundle id |
| emu with its own Metal cache (bundle `dev.simee.emu`): repeat boots of one binary at one path, renderer init / launch to `sys.boot_completed`, median (runs) | 1.01 s (4.69, 1.01, 0.35) / 11.7 s (20.0, 11.7, 8.3): the shared cache missed in 2 of 3 boots | 0.34 s (0.33 to 0.36) / 7.8 s (7.8, 7.8, 7.8, 9.8, then 7.8, 8.4, 8.0 in a third interleaved round), 7 boots (2 with the ring-yield experiment of android.md "Ring polling", since dropped); the third round's before arm, run the same minute, was warm too (init 0.32 to 0.36 s, boot 7.8, 7.8, 9.4 s, median 7.8), so that round did not reproduce the shared cache's misses | API 36 phone 720×1280, 4 vCPUs, M3 Max, 2026-11-26, arms interleaved, both binaries warmed first; host load 8 to 33 (before) and 9 to 22 (after), 15 to 17 in the third round (2026-11-26, 08:57 to 08:59); `emu -verbose` (`renderer initialized in`), `adb shell getprop sys.boot_completed` polled every 0.25 s from launch |
| Same boots, launcher swipes 30 s after `sys.boot_completed`: janky frames / emu CPU, median (runs). Not a difference: both arms swung run to run on a host other work was loading (the 1-minute load reached 114 during the run) | 3.1% (3.1, 1.1, 27.0) / 30% (28, 30, 55) | 1.1% (0, 20.4, 0, 2.3) / 34% (28, 38, 30, 38) | same runs, `scripts/android-jank.sh emulator-5600 <pid>` |
| A fresh install path: the boot-shaped renderer init (`emu -warm-renderer <lib64>`, timed) without / after `emu -provision-renderer` from that path, 3 interleaved rounds | 5.42, 0.83, 0.73 s (cold once: later fresh copies were warm) | 0.78, 1.00, 0.78 s | M3 Max, 2026-11-26, host load 180 to 210 (every number slow), a new copy of one emu build per run |
| Host ANGLE on SwiftShader when the renderer runs without GL (was Metal): the boot-shaped renderer init without a guest, Metal's shader cache cold / warm, median of 5 (range) | 4.59 s (4.51 to 4.64) / 0.35 s (0.34 to 0.38) | 1.54 s (1.52 to 1.65) / 0.35 s (0.35 to 0.38) | M3 Max, macOS 26.6.2, 2026-11-26, host load 8.3 to 9.3, SDK renderer 37.2.13, 5 interleaved rounds, `emu -warm-renderer <lib64>` timed; each binary under its own bundle id, cold = that bundle's cache removed |
| Same change, API 36 phone (720×1280, 4 vCPUs) boots with Metal's cache cold: renderer init / boot waited for it / launch to `sys.boot_completed`, median of 5 | 4.51 / 4.49 / 15.06 s (15.04 to 15.62) | 1.48 / 1.47 / 12.10 s (12.07 to 12.11) | same host and renderer, 2026-11-27, host load 4.4 to 20, rounds interleaved, `emu -verbose` (`renderer initialized in`, `boot waited`), `adb shell getprop sys.boot_completed` polled every 0.25 s from launch. The rest of a cold boot's 4.6 s over a warm one isn't the renderer's init; not attributed, likely the guest's own pipelines compiling through MoltenVK |
| Same boots, Metal's cache warm: renderer init / boot waited / launch to `sys.boot_completed`, median of 5 | 0.325 / 0.313 / 7.44 s | 0.333 / 0.319 / 7.45 s | same runs |
| Same change, launcher swipes 30 s after `sys.boot_completed` on a warm boot: janky frames / p99 / emu CPU, median of 3 | 0% / 8 ms / 26% of a core | 0.13% / 9 ms / 25% | same AVD and host, load 5.1 to 6.9, arms alternated with each boot after one that ran swipes, `scripts/android-jank.sh emulator-5650 <pid>`; a first set where each before boot followed a boot stopped at `boot_completed` drew 324 to 526 frames in 3 of 5 before runs and isn't used |
| Same change, emu RSS after those swipes, median of 8 | 3819 MB | 3835 MB (SwiftShader's library and idle workers) | same runs |
| Ring polling on the warm boots: render-thread samples yielding in `RingStream::readRaw` (`swtch_pri`), in fractions of a core during swipes, median of 3 | 0.57% (0.53 to 0.68) | 0.48% (0.47, 0.48, 3.79) | same runs, `sample <pid> 20 1` during a second `scripts/android-jank.sh` run |
| Plain `fsync` for guest flushes: first-boot APEX decompression | over 270 s | about 70 s | M-series Mac |
| Screenshot of a 2480×1740 screen (convert and encode) | 86 ms | 19 ms | `cargo run --release -p simee-core --example frame_bench` |
| Display stream: emulator CPU per frame besides the readback, tablet / phone | 12.8 / 7.1 ms | 2.0 / 0 ms | 4-core Linux VM |
| vsock host to guest | 474 MiB/s | 597 MiB/s | 4-core Linux VM, `vsock_throughput` |
| adb client to guest, end to end | 498 MiB/s | 870 MiB/s | 4-core Linux VM, `adb_throughput_to_guest` |
| Whole-disk FITRIM | | 90 ms | M-series Mac |
| Vulkan-only renderer: warm boot of a 1080×2400 phone, median (runs) | 3.7 s (3.7, 3.5, 3.7) | 3.3 s (3.3, 3.3, 3.3) | M3 Max, 2026-11-25, `EMU_RENDERER_GL=1 scripts/android-bench.sh` against without |
| Vulkan-only renderer: peak RSS of that boot | 3262 MB (3279, 1939, 3262) | 2014 MB (2044, 2014, 1945) | same runs |
| GPU state released on reset: hard guest reset of a 720×1280 API 36 phone, trigger to the next boot's first host frame, median (runs) | never (boot not complete in 120 s) | 3.26 s (3.36, 3.12, 3.26) | M3 Max, 2026-11-25, host load 7 to 9, `scripts/android-reset-bench.sh <avd> hard 3` |
| Same, trigger to `sys.boot_completed` | never | 2.96 s (3.13, 2.96, 2.94) | same runs |
| Same change, `adb reboot`: trigger to first host frame / to `sys.boot_completed`, median of 8 (two processes, 4 reboots each) | 3.92 / 3.74 s (3.72 to 4.12 / 3.61 to 3.99) | 4.06 / 3.88 s (3.65 to 4.18 / 3.61 to 4.08) | same AVD and hour, builds interleaved, host load 6 to 10, `scripts/android-reset-bench.sh <avd> reboot 4` |
| Same, a second interleaved set at host load 13 to 66 | 4.07 / 3.97 s (3.78 to 4.74 / 3.78 to 4.58) | 4.07 / 3.91 s (3.84 to 4.23 / 3.78 to 4.06) | same AVD and hour |
| Same change, `kill -INT` to process exit (exit status) | 0.13 s (0.14, 0.12, 0.13; all 0) | 0.13 s (0.13, 0.14, 0.13; all 0) | same AVD and hour, host load 6 to 8, `scripts/android-reset-bench.sh <avd> exit 3` |

The reset rows come from `scripts/android-reset-bench.sh` with `EMU` set to each build (the
change's parent and the change, release builds, 16 cores). A hard reset is
`echo b > /proc/sysrq-trigger` under `adb root`. Every time is from the trigger, so it includes
what emu does between the old boot's last instruction and the new boot's first (with this change,
the GPU teardown and the black frame); for `adb reboot` it also includes the guest's orderly
shutdown, about 1.0 s on both builds. The first host frame is the first `adb emu screenshot`
after emu's `-verbose` log prints `stopped after …: Reset` (polled every 0.1 s) that is neither
the frame taken just before the trigger nor all black: the next boot's first content, by the
same test on both builds (before the change the screen kept the last boot's final frame until
then, after it black). `sys.boot_completed` is polled beside it. Before the change a hard reset
never booted; a frame that differed did show (0.60 s), but not the next boot's. For
`adb reboot` the two sets disagree in sign (+0.14 / +0.13 s, then 0.00 / -0.06 s), within
the spread between rounds of one build (0.18 s): no cost is measured. These rows replace runs
that timed from the Reset line, which this change prints after its own teardown, ran the
0.3 s all-black test on every poll of a black screen (so only the build that shows black
polled slowly), and timed `sys.boot_completed` only after the first frame; those runs had
`adb reboot` about 0.3 s slower with the change.

The `--ignored` benches run on any host:

```bash
cargo test --release -p simee-emu-vmm vsock_throughput -- --ignored --nocapture
cargo test --release -p simee-emu-vmm present_cost -- --ignored --nocapture
CASE=2480,1080,true cargo test --release -p simee-emu frame_path_latency -- --ignored --nocapture
```

## iOS

The injected network shaper's cost per call is measured in
[research/netshape-overhead.md](research/netshape-overhead.md).

### `launch`, with and without `restart`

iPhone 17 Pro, iOS 27.2 (24B8117), `com.apple.Preferences`, through a release `simee serve`,
median of 10, host load 9–18 (2026-11-27, M3 Max). Reproduce:
`python3 -I scripts/bench/ios-api.py --suite launch`.

| Case | ms |
| --- | --- |
| Not running (1 s after `terminate`), `restart` true or false | 32 |
| Right after `terminate` | 122 |
| Right after `terminate`, after waiting for the old PID to exit | 121 |
| Running, `restart: true` (killed and cold-started, new PID) | 136 |
| Running in the background, `restart: false` (same PID, brought forward) | 29 |
| Running in front and settled, `restart: false` | 17 |
| 1 s after a cold launch, `restart: false` | 2120 |

The old process has already exited when `terminate` returns, so waiting on its PID saves
nothing. The service instead holds a launch for about 100 ms after a terminate (129 ms at a 0 ms
gap, 73 ms at 50 ms, 32 ms from 100 ms on). `restart: false` waits for a launch or activation
still in progress: about 3.15 s after a cold launch began, up to ~0.8 s after one brought the app
forward.

### Per-call device lookup on a crowded host

iPhone 17 Pro, iOS 27.2 (24B8117), booted, on a host with 90 simulators and 112 AVDs, through a
release `simee serve`, one persistent connection, median of 20 round trips over the socket (the
median of 3 runs, the builds before and after #120 interleaved against the same device), 1-minute
load 10-23 (2026-11-26, M3 Max, macOS 26.6). "Before" sorted every simulator
(`-[SimDeviceSet devices]`) to find one by UDID, listed the AVD home to route each call, and read
the framebuffer size through a remote proxy on every `screen` and tap; "after" looks the UDID up
in the set's dictionary (`devicesByUDID`), stats one `<name>.ini`, and keeps the size per boot
until a rotation or fold. Reproduce: `python3 -I scripts/bench/ios-api.py --suite lookup --calls 20
--baseline <before build>` (the crowded host's 90 simulators and 112 AVDs are not created for you).

| Method | before (ms) | after (ms) |
| --- | --- | --- |
| `location` | 1.54 | 0.66 |
| `screen` | 0.34 | 0.04 |

What `location` has left is the service's own round trip (`host_support_mig_set_location_lat_lon`,
~95% of its samples).

### Android device resolution

How long a call spends finding its device (#256), with 44 AVDs (phones, tablets, watches, TVs and
cars, each with the `config.ini` avdmanager writes) and, in the second half of the table, two
stand-ins for the stock emulator running (a process with a qemu's command line, `-port` from 5900,
holding the AVD's `hardware-qemu.ini.lock`). Linux, 4-vCPU Xeon @ 2.20 GHz VM, 7.8 GB, Ubuntu
22.04, 1-minute load under 3 (2026-11-28). **Not measured on macOS or with a real emulator.**
Medians over 5 rounds of the p50 and p95 of 100 calls, the build before and after alternating.
"Kept" is one connection to a `simee serve` that has been up a second; "one-shot" is a fresh
`simee` process with no server on the socket, so it includes the process's start (`--version` is
the floor: 4.7 ms). The calls fail, and the failure is what is timed, at different points:
`tap` of an exact name fails after the device is resolved and before any network I/O (a stopped
AVD isn't running, and a stand-in is a stock emulator, which refuses input over the display
socket); `booted` fails inside resolution, with no running device ("no device matches `booted`")
or, with 2 running, because it "matches several devices", so no device is picked and nothing
after the lookup runs (`info booted` and `tap booted` in the 2-running column). Reproduce:
`python3 -I scripts/bench/android-resolve.py --baseline <before build>`.

| Call | none running, before → after (ms) | 2 running, before → after (ms) |
| --- | --- | --- |
| kept: `info` of an exact AVD name, p50 / p95 | 0.33 → 0.13 / 0.37 → 0.17 | 0.25 → 0.14 / 0.31 → 0.19 |
| kept: `info` of `booted`, p50 / p95 | 2.10 → 0.36 / 2.39 → 0.40 | 2.02 → 0.39 / 2.23 → 0.52 |
| kept: `devices` with `booted`, p50 / p95 | 2.20 → 0.80 / 2.36 → 1.90 | 2.07 → 0.85 / 2.30 → 1.49 |
| kept: `devices`, p50 / p95 | 3.25 → 2.40 / 5.87 → 2.83 | 3.30 → 2.43 / 5.90 → 2.78 |
| one-shot: `tap` of an exact name, p50 / p95 | 6.89 → 6.54 / 7.42 → 6.71 | 21.2 → 6.7 / 22.3 → 6.9 |
| one-shot: `tap booted`, p50 / p95 | 11.7 → 7.9 / 12.0 → 8.4 | 32.7 → 8.2 / 36.4 → 8.7 |
| one-shot: `list --booted`, p50 / p95 | 11.6 → 11.0 / 12.7 → 11.2 | 32.3 → 11.2 / 34.9 → 11.8 |
| one-shot: `list`, p50 / p95 | 12.1 → 11.5 / 13.3 → 11.7 | 33.1 → 11.7 / 35.8 → 12.3 |

What changed: the stock emulator's process facts (command line, listening ports) are read for the
one PID a lock names, from `/proc/<pid>` on Linux and libproc on macOS, instead of a host-wide
`ps -axo` and `lsof` (~60 ms each on a macOS host with ~1300 processes; `ps` here, with a few
hundred processes, is ~20 ms). A kept server remembers them by (PID, start time). Each AVD's pointer
and `config.ini` are parsed once and then validated by one `stat` (path, mtime, ctime, size, inode); an
exact name is one `stat` of its pointer instead of listing the home; `booted` loads only the
running AVDs' configs.

What is left, measured. Kept `devices` is still about 2 ms (p50 2.01 ms with nothing running and
1.93 with 2 running, p95 2.4 and 2.6; 2.01 and 2.20 on the build before the listing work below,
which is no difference at this sample size: the per-round p50s of the same build range over
0.5–2.3 ms, so none of the kept-connection rows moved by more than noise). Where the time goes in
the server process on the same VM, p50 over 1000 calls, from the `listing_cost` test
(`python3 -I scripts/bench/android-resolve.py --make-home DIR`, wait 3 s, then
`SIMEE_BENCH_SDK=DIR/sdk SIMEE_BENCH_AVD_HOME=DIR/avd cargo test --release -p simee-android
listing_cost -- --ignored --nocapture`): listing the home 0.005 ms, resolving each of the 44 AVDs'
pointer and directory 0.11 ms, each one's lock 0.07 ms, describing each (config `stat`, image
`stat`, the profile and the `DeviceInfo`) 0.23 ms, the whole `devices` 0.44 ms. The same test cut
to `list_avds` and `devices` on the build before (`main` at #383, a copy of the test) gives 0.05–0.10
and 0.59–0.62 ms, so the listing work is worth about 0.15 ms in the server. The rest of the 2 ms
is the 10.7 KB reply: a call that does the same work and returns `[]` (`devices` with `booted`)
takes 0.6–0.7 ms over the socket, and `help`, which computes in 0.1 ms, takes ~0.9 ms to arrive on
this VM. So the listing is under 1 ms of server work and the call is not; getting the call under
1 ms here is a matter of the connection's reply path (`crates/sim-api/src/lib.rs`), which was not
changed. The listing's floor with exact validation is 5 system calls per AVD (pointer, directory,
config, image, lock). The remembered listing of the home is not used when a pointer is a link or
any read failed. `devices` with `booted` is 0.63 / 0.74 ms (none / 2 running) and `info` of
`booted` 0.44 / 0.35 ms, also within noise of the build before.
A cold one-shot `list` or fuzzy name
reads every AVD's two files (about 700 system calls for 44 AVDs, ~5 ms over the floor); an on-disk
cache would still `stat` each file, and wasn't built. With nothing running, the one-shot exact-name
`tap` is within 1.7 ms of `--version`.

### Introspection: `processes`, `connections`, `kill`, `network_state`

iPhone 17 Pro, iOS 27.2 (24B8117), freshly booted (~205 device processes, ~1200 on the host),
through a release `simee serve`, median of 10 round trips over the socket, the build before and
after #118 against the same device back to back, host load 19–34 (2026-11-27, M3 Max, macOS
26.6.2). `kill` is of a just-launched `com.apple.Preferences`. Host children counted by polling
the server's `proc_listchildpids` every 0.2 ms over 5 calls. Reproduce the times:
`python3 -I scripts/bench/ios-api.py --suite introspection --baseline <before build>`; the spawn
counts were polled by hand and have no script.

| Method | `ps`/`lsof`/`kill` (ms) | libproc (ms) | Host spawns per call, before → after |
| --- | --- | --- | --- |
| `processes` | 141.6 | 3.5 | 2 → 0 |
| `connections` | 190.7 | 2.5 | 3 → 0 |
| `kill` | 126.3 | 2.5 | 2 → 0 |
| `network_state` | 385.4 | 198.6 | 3 → 0 |

For scale, `crash_reports` (a device lookup and a directory listing) takes 1.5 ms on the same
server; the libproc reads themselves take 0.9 ms (`processes`) and 0.5 ms (`connections`) in
process. `network_state`'s remaining
time is its in-device `launchctl getenv` (for `injected`), which runs under the device's
`launchd_sim`, not the server.

### iPad `pointer` click

iPad Pro 13-inch (M5), iOS 27.2 (24B8117), the InputProbe test app (it logs where each click
lands), `pointer` `action: click` through a release `simee serve`, one persistent connection,
20 targets spread over the screen; a fresh server for each of 10 runs (its first click homes),
the first run clicking all 20 targets and the other 9 three. Before and after #122 built from the
same tree into separate target dirs, against the same device back to back (2026-11-27, M3 Max,
macOS 26.6.2). Quiet is host load 9-37 from other work; loaded adds 32 `yes` processes (load
14-96). Before #122 every call homed the pointer; now a server moves on from where its last call
left it, in steps of up to 120 pt instead of 10 pt, and homes on its first call, after a failure
or rotation, and every 100 calls. Reproduce the times: `python3 -I scripts/bench/ios-api.py --suite
pointer --baseline <before build>` (targets spread over the screen with Settings in front). The
placement column needs the InputProbe test app, which isn't in the repository: no script.

| Case | before (ms) | after (ms) |
| --- | --- | --- |
| First click (homes), quiet, median of 10 | 3493 | 1839 |
| Later click, quiet, median of 37 | 3641 | 408 |
| First click (homes), loaded, median of 10 | 3638 | 1849 |
| Later click, loaded, median of 37 | 3803 | 412 |

All 20 clicks landed on their targets in every case: within 0.36 pt (median 0.28) before and
0.10 pt (median 0.00) after, quiet and loaded, read back from the app's log. 120 later clicks to
random points after one homing stayed within 0.32 pt. A later click is now the move (23
reports at most, 16 ms apart, sent against deadlines), 64 ms for the move to land and the 80 ms
press.

### iOS input defaults

iPhone 17 Pro, iOS 27.2 (24B8117), through a release `simee serve`, 2026-11-27, M3 Max (16
CPUs), macOS 26.6. The test app has a UIKit button, a SwiftUI button, a link in a WKWebView, a
UITableView and a UITextField; it logs each one that fires, every touch its window sees (with the
touch's timestamp) and when it goes to the background. "Loaded" adds `yes` processes: 32 for
recognition (1-minute load 20-78), 48 for the timings (load 94-117, above 3x the CPUs); quiet
is load 5-60 from other work while the load average decayed. The recognition and typing tables
need that test app (not in the repository): no script. The per-call timings:
`python3 -I scripts/bench/ios-api.py --suite input --baseline <before build>` (tap, `button
home`, `key`, `type` and `swipe`, with Settings in front instead of the test app).

Recognition, 20 taps or presses per cell, every cell the same quiet and loaded:

| Hold | UIKit button | SwiftUI button | WKWebView link | table row | `home` | `lock` |
| --- | --- | --- | --- | --- | --- | --- |
| 0 ms (down and up back to back) | 20/20 | 20/20 | 20/20 | 20/20 | 20/20 | 20/20 |
| 5, 10, 16 and 30 ms | 20/20 | 20/20 | 20/20 | 20/20 | 20/20 | 20/20 |

Typing a 41-character string with capitals, digits and punctuation (`Teh quick, Brown FOX's 42
jumps: ok? Yes!`) and Return into the text field, 10 times per gap; autocorrect turns it into
`The quick, Brown FOX’s …` every time at 30 ms a key (before) and at every gap below from 1 ms:

| Gap between key reports | quiet | loaded (32) |
| --- | --- | --- |
| 0 ms | 9/10 (one run lost its keys) | 10/10 |
| 1, 2, 3 and 5 ms | 10/10 | 10/10 |

Repeated keys at the 3 ms gap (a key goes up and the same key down again 3 ms later), into a
field that corrects nothing (no autocorrect, capitals, spelling or smart punctuation), 10 runs
each, compared exactly; once at 1-minute load 20-22 and once at 123-167 (other lanes' devices
booting):

| String | load 20-22 | load 123-167 |
| --- | --- | --- |
| `hello` | 10/10 | 10/10 |
| `1000` | 10/10 | 10/10 |
| `pass11` | 10/10 | 10/10 |
| `aaabbb` | 10/10 | 10/10 |
| `Mississippi` | 10/10 | 10/10 |

The defaults are now a 10 ms `tap` hold, a 10 ms hold for `button` `home` and `lock` on an iPhone
or iPad (every other button, Apple TV and Apple Watch keep 80 ms: not measured), and a key report
every 3 ms for `type` and `key`,
sent against deadlines from the first. 10 ms and 3 ms leave a margin over the shortest holds and
gaps that passed everywhere (0 ms and 1 ms). Every iOS touch gesture (`tap`, `swipe`, `touch`,
`stylus`, `remote_swipe`, CarPlay touches) and the Digital Crown now send their samples against
deadlines too ([`sim_core::gesture::pace`](../crates/sim-core/src/gesture.rs)). An explicit
`hold_ms` or `duration_ms` works as before. Per call, median of 10 over one persistent
connection, timed as `scripts/ios-step-bench.py` times its calls, the build before (in its own
target dir) and after against the same device back to back:

| Call | before, quiet | after, quiet | before, loaded | after, loaded |
| --- | --- | --- | --- | --- |
| `tap` (default hold) | 64.5 ms | 12.6 ms | 65.0 ms | 12.9 ms |
| `button home` (default hold) | 84.1 ms | 13.6 ms | 89.0 ms | 13.9 ms |
| `key` | 37.0 ms | 6.9 ms | 38.3 ms | 8.0 ms |
| `type`, 10 characters | 375.8 ms | 61.2 ms | 395.0 ms | 61.4 ms |
| `type`, 20 characters | 749.4 ms | 121.2 ms | 782.9 ms | 121.2 ms |
| `type`, 20 with capitals and punctuation | 883.4 ms | 151.2 ms | 919.7 ms | 151.4 ms |
| `swipe`, `duration_ms` 300 | 375.8 ms | 320.6 ms | 422.0 ms | 320.7 ms |
| `swipe` 300, touch down to last move as the app saw it | 355.4 ms | 302.1 ms | 397.9 ms | 302.7 ms |

A 300 ms swipe's motion now takes 300 ms within 1% quiet and loaded (it took 18% and 33% longer);
the call ends one sample step (17 ms) after the last move, when the finger lifts, as planned.

## Desktop: Rune's native Metal backend against wgpu

What the desktop window costs on wgpu's Metal backend (`SIMEE_GPU_BACKEND=wgpu`), which the 3D
device view needs (Rune's native Metal backend has no depth attachments or MSAA resolves,
anomalyco/rune#2547), against Rune's native Metal backend, the default. The flat view is the same in
both. Apple M3 Max (Mac15,9, 16 CPUs), macOS 26.6.2, Rune `f6f6e04`, 2026-11-27. Each value is the
median of 5 runs per backend, run alternately (metal then wgpu, then wgpu then metal). The 1-minute
load average at the start of the runs was 5 to 11 for iOS and 8 to 22 for Android. Two Android
pairs that started at load 48 to 140 were run again. CPU and GPU are percentages of one core and of
the GPU. GPU time is the process's `accumulatedGPUTime` from `ioreg -c AGXDeviceUserClient`.

| Measure | iOS: Metal | iOS: wgpu | Android: Metal | Android: wgpu |
| --- | --- | --- | --- | --- |
| Launch to first presented frame | 111 ms | 106 ms | 117 ms | 119 ms |
| Idle CPU (static screen, 20 s) | 1.00% | 1.10% | 0.65% | 0.60% |
| Idle GPU | 0.63% | 0.66% | 0.00% | 0.00% |
| CPU during swipes (20 swipes, 20 s) | 10.0% | 11.6% | 20.5% | 18.8% |
| GPU during swipes | 8.4% | 9.2% | 11.3% | 10.8% |
| Frames drawn during swipes | 1,277 | 1,265 | 1,363 | 1,364 |
| Frame time (start to present), p50 / p95 | 1.24 / 1.51 ms | 1.34 / 1.58 ms | 1.45 / 2.47 ms | 1.26 / 1.89 ms |
| Gap between frames during swipes, p50 / p95 | 9.3 / 18.4 ms | 9.4 / 20.5 ms | 9.8 / 24.0 ms | 9.9 / 23.9 ms |
| Resident memory after the swipes | 126 MiB | 134 MiB | 164 MiB | 176 MiB |

Devices: an iPhone 17 Pro on iOS 27.2 (simulator), and an API 36 `google_apis` phone at
1080x2400 on emu. The iOS window draws about 3 frames a second while idle and the Android one
none, on both backends. In window screenshots (`screencapture -l`), all 10 iOS idle shots are
byte-identical across backends, and so are the Android shots taken in the same device state. Shots
after a swipe, Home, Rotate (portrait to landscape and back, Safari and Android Settings) and the
bezel match byte for byte, except where the device's own content changed between runs (Safari's
favicons loading, a notification's age). Input through the window works the same on both backends.

The measuring build is `cargo build --release --features tooling,rune/frame-profile` (Rune's
per-frame log, `RUNE_FRAME_LOG`; `--window-script` replays the swipes as window drags). Each run
starts the window with `RUNE_GPU_BACKEND=<metal|wgpu> SIMEE_DEVICE=<device>`. Run times are counted
from the first frame: idle from 10 to 30 s, a screenshot at 31 s, then swipes from 32 to 52 s, one
drag on `device-screen` a second (iOS left and right, Android up and down). The shipped build
without `frame-profile` was not measured.

No script: the runs open the desktop window on the host's display with a device booted, drive it
with a Rune `--window-script` file and read Rune's per-frame log and `ioreg`; the script files and
the log parser used weren't checked in. The setup above is what to rebuild.

## Video encode (H.264, VideoToolbox)

`simee-encode` turning a device's frames into H.264 for the WebRTC publisher: 10 s per run with a
swipe every 0.7 s, medians of 3 rounds, defaults (Constrained Baseline, 4 Mbit/s average, IDR every
2 s), 2026-11-27, Apple M3 Max (Mac15,9), macOS 26.6.2, hardware encoder. Reproduce:
`scripts/bench/encode.sh --udid <booted iPhone>` or `--avd <running AVD>`. The method, the full
tables (latency p95, picture step, output rate, IDR sizes) and what the numbers say are in
[streaming.md](streaming.md#encode-cost); the tablet and iPad rows are not measured yet.

| Device, size, fps cap | Encoded, fps | Encode latency p50, ms | Encoding process CPU, % of a core |
| --- | --- | --- | --- |
| iPhone 17 Pro, 1206x2622, 30 | 26.8 | 17.28 | 2.7 |
| iPhone 17 Pro, 1206x2622, 60 | 46.4 | 17.20 | 4.0 |
| iPhone 17 Pro, 588x1280, 30 | 26.9 | 6.07 | 2.8 |
| iPhone 17 Pro, 588x1280, 60 | 46.8 | 6.06 | 4.2 |
| Android API 36, 1080x2400, 30 | 26.9 | 14.51 | 8.5 |
| Android API 36, 1080x2400, 60 | 45.7 | 14.45 | 9.8 |
| Android API 36, 576x1280, 30 | 22.1 | 5.80 | 6.9 |
| Android API 36, 576x1280, 60 | 37.3 | 5.77 | 8.1 |

## Video encode (H.264, OpenH264 on Linux)

`simee-encode`'s Linux encoder (OpenH264 built from source, `--features openh264`) on synthetic
phone screens, no device: an app drawer scrolling on a 1080x2400 screen presented at 60 fps,
encoded through the real loop, 10 s per run, medians of 3 rounds, defaults (Constrained Baseline,
4 Mbit/s average, IDR every 2 s), OpenH264 built with `nasm`, 2026-11-27. Host: a Linux VM,
Intel Xeon @ 2.20 GHz, 4 vCPUs, 3.8 GB, Ubuntu 22.04, kernel 6.1.189. Reproduce:
`scripts/bench/encode-linux.sh` (`OPENH264_NO_ASM=1` for the no-`nasm` row). Latency includes the
colour conversion and scaling. The method, PSNR, bitrate and IDR sizes, and what the numbers say
are in [streaming.md](streaming.md#encode-cost-on-linux). Synthetic screens, so a real device's
bitrate will differ; this VM's numbers are not comparable with the Apple M3 Max rows above, and
aarch64 speed is not measured.

| Screen, size, fps cap | Encoded, fps | Encode latency p50, ms | Process CPU, % of a core |
| --- | --- | --- | --- |
| 1080x2400, 30 | 28.8 | 26.12 | 82 |
| 1080x2400, 60 | 28.4 | 36.54 | 99 |
| 1080x2400 capped at 1280 (576x1280), 30 | 29.8 | 22.39 | 68 |
| 1080x2400 capped at 1280 (576x1280), 60 | 58.7 | 14.17 | 86 |
| 1080x2400, 30, built without `nasm` | 13.6 | 61.82 | 100 |
| 1080x2400 capped at 1280, 30, built without `nasm` | 24.6 | 35.00 | 98 |

## MCP server

What the MCP server's protocol layer costs to build, against the official Rust SDK. Linux VM,
4 vCPUs, 2026-11-26, `CARGO_BUILD_JOBS=2`, rustc 1.99.1.

| Measure | `sim-mcp` (hand-written, on `serde_json`) | `rmcp` 3.11 |
| --- | --- | --- |
| Crates added to `Cargo.lock` | 1 (`sim-mcp`) | 75 in the normal dependency tree |
| Clean dev build of a program that depends on it, empty `main` | not measured alone: 0 new dependencies to build | 41 s wall (62 s user, 11 s system) |

`rmcp` was built from a scratch package with `rmcp = { version = "3.11", default-features = false,
features = ["server", "transport-io"] }` and `tokio` with `rt`, `macros` and `io-std`; the crate
count is `cargo tree --prefix none -e normal | sort -u | wc -l` in it (145 with its dev
dependencies). `Cargo.lock` has 140 packages in all. Reproduce: `scripts/bench/mcp-build.sh`
(it downloads `rmcp` into cargo's cache; nothing is added to this repository). Why this decides the protocol layer:
[mcp.md](mcp.md#why-the-protocol-layer-is-hand-written).

## MCP idle subscription

What a subscribed resource costs while nothing changes: the watch thread's own wake-ups and CPU
(`/proc/self/task/<tid>`), over 10 s after a 0.5 s settle, against a fake backend that answers at
once, so it is the watcher and not the device. Linux VM, 4 vCPUs (Xeon @ 2.2 GHz, Linux 6.1),
2026-11-27, release build, one run each (the numbers are counts, not timings). "Before" is
`main` at 6ad2666, whose watcher slept in 10 ms steps and compared the clock, with the same test
body copied in (the old `spawn` took one interval, 1 s).

| Subscribed to | Wake-ups a second, before | after | CPU in 10 s, before | after | Reads in 10 s |
| --- | ---: | ---: | ---: | ---: | ---: |
| one screen | 98.5 | 1.0 | 40 ms | 0 ms (under the 10 ms tick) | 11 |
| the resource list | 98.2 | 1.0 | 40 ms | 0 ms | 11 |
| one screen and the list | 98.5 | 1.0 | 40 ms | 0 ms | 22 |

The reads are the real cost on a device: a subscribed screen is one `screenshot` a second at the
default pace (`SIMEE_MCP_SCREEN_FPS` raises or lowers it), and each is the call timed in
[Time per automation step](#ios-simee-against-simctl). The host CPU of a subscribed screen on a
device is [not yet measured](#not-yet-measured). A resource nobody subscribed to costs nothing:
the server has no thread and makes no read (a test counts the backend's calls).

## Build times (local)

Apple silicon Mac, 16 CPUs, macOS 26.6, 2026-11-26. The host was heavily loaded by other builds
the whole time (load average 50 to 700), so wall-clock time is not comparable between runs; the
rows give CPU seconds (user plus system of the build and its children, from `getrusage`), which
load does not inflate. Each is a clean build into an empty target directory; "before" and
"after" were run alternately, twice, and the range is both runs. Wall-clock times under that load
were 20 to 34 s before and 10 to 14 s after for the workspace, 114 s before and 37 to 42 s after for the desktop.

| Build | Before | After | Command |
|---|---|---|---|
| Workspace test build, clean | 131 to 136 CPU s | 74 to 77 CPU s | `cargo test --no-run` |
| Desktop test build, clean | 614 CPU s | 147 CPU s | `cd desktop && cargo test --no-run` |
| Workspace test run, warm | 12.5 to 13.7 s | 13.0 to 13.9 s | `cargo test --workspace` |
| Desktop test run, warm | 0.55 to 0.99 s | 1.06 to 1.41 s | `cd desktop && cargo test` |

Reproduce the clean builds: `scripts/bench/build-times.sh --only "workspace desktop"` ("before" is
#57's parent, from its own checkout).

What changed: the workspace's `dev` profile built every dependency at opt-level 2; it now builds
them at 0 and keeps 2 or 3 only for the ones on a runtime path (zlib-rs and flate2 for the kernel's
gunzip, smoltcp, crc32fast, adler2, base64, cpal, and the PNG crates that already had it). The
desktop's `test` profile builds its dependencies (Rune's GPU stack) at 0: its view tests run in
about a second either way. `cargo build` and `cargo run` in the desktop keep opt-level 2, since a
debug window has to stay smooth.

Measured and left alone:

| Build | Result | Command |
|---|---|---|
| Release `emu`, clean | 13 s wall (57 CPU s) at load 17; release settings unchanged | `cargo build --release -p simee-emu` |
| Incremental after a one-line edit in `sim-core`, tests | 3.0 to 4.9 s; unchanged by `debug = 0` (3.1 to 3.6 s) or `-ld_classic` (3.6 to 4.1 s) | `cargo test --no-run` |
| Incremental after a one-line edit in `emu-vmm`, `cargo c` / `cargo lint` | about 1 to 2 s each | `cargo c`, `cargo lint` |
| `scripts/check.sh --all`, warm | 15 to 26 s, of which the test run is 13 to 22 s | `scripts/check.sh --all` |

The incremental loop is already a few seconds, so no profile change helps it; the clean-build wins
are what a fresh checkout, a CI cache miss or a Rune bump pays.

## iOS: the slim profile's jobs (62 against 74)

Moved from [farm.md](farm.md#the-slim-profile), which explains which jobs and why.
`boot --profile slim --settle`, fresh clones of a booted-once template, the 62-job profile against
the 74-job one (#222), 3 interleaved rounds each, 2026-11-27, M3 Max (16 CPUs, macOS 26.6), 1-minute
load 8-21, median (runs):

| runtime, settle judged on | | settled | CPU-s to settle | background CPU-s | processes |
| --- | --- | --- | --- | --- | --- |
| iOS 27.2, foreground (default) | 62 jobs | 33.2 s (40.3, 32.2, 33.2) | 112.0 | 25.1 | 114 |
| | 74 jobs | 20.1 s (20.1, 20.1, 22.3) | 82.3 | 8.6 | 95-96 |
| iOS 27.2, all CPU | 62 jobs | 38.9 s (38.9, 26.2, 39.5) | 114.6 | 26.2 | 108-113 |
| | 74 jobs | 30.2 s (20.0, 30.3, 30.2) | 82.0 | 10.3 | 93-94 |
| iOS 18.6, foreground (default) | 62 jobs | 25.5 s (20.8, 30.6, 25.5) | 42.1 | 13.3 | 91-97 |
| | 74 jobs | 19.7 s (29.3, 19.7, 17.5) | 36.6 | 9.4 | 79-84 |
| iOS 18.6, all CPU | 62 jobs | 31.5 s (30.7, 31.5, 31.6) | 44.0 | 14.5 | 97-102 |
| | 74 jobs | 19.4 s (19.4, 19.6, 18.5) | 33.2 | 8.2 | 79 |

```bash
scripts/bench/ios-settle.sh --baseline <62-job build> --settle-cpu foreground   # and --settle-cpu all
scripts/bench/ios-settle.sh --baseline <62-job build> --runtime com.apple.CoreSimulator.SimRuntime.iOS-18-6
```

The settle tables of [farm.md](farm.md) (fresh devices and clones at N = 1 to 8, the K sweep,
warm reboots) were measured with `scripts/farm-bench-ios.sh` as farm.md's "Benchmark" section
gives; `scripts/bench/ios-farm.sh` runs its clone mode with a template it creates.

## iOS: app preferences

Moved from [ios.md](ios.md#app-preferences) (#178), which describes the backend. iPhone 17
Pro, iOS 27.2, M3 Max, 2026-11-27, a test app reading and writing an app-group suite, through
`simee serve`, round trips over the socket:

| Call | Median |
| --- | --- |
| `prefs_read` of one store | 0.11 ms (n=20) |
| `prefs_read` of every store (3) | 0.27 ms (n=10) |
| `prefs_stores`, warm | 0.13 ms (n=10) |
| `prefs_stores`, first call in a process | 133 ms (n=1) |
| `prefs_read {fresh: true}` (one in-device spawn) | 182 ms (n=5) |
| A 10-key write batch | 180-187 ms; the running app had every key 1.7 ms after the call returned |
| Feed: cfprefsd writes the file to the feed's event | 1.0 ms (0.8–1.1, n=6) |
| Feed: an in-app write to the event | 1.5 s from the newest write a flush covers, 8.5 s from the oldest (n=6): cfprefsd flushes about every 10 s |
| Feed end after `simctl shutdown` begins | 3.5 s |

Reproduce the reads: `python3 -I scripts/bench/ios-api.py --suite prefs --bundle <app>` (default
Settings, whose stores differ from the test app's). The writes and the feed need the test app
ios.md describes, which isn't in the repository: no script.

## iOS: memory tools

Moved from [api.md](api.md) (#183). Apple M3 Max, 16 CPUs, macOS 26.6.2, iOS 26.5 iPhone 17
simulator, 2026-11-27; a small test app with 20 leaks, 13 k heap nodes and 1.8 MB malloced,
launched with stack logging; median of 3 warm-server calls; wall from `/usr/bin/time` on `simee
call`, CPU the server's user and system time including the tool it reaped:

| Method | Wall | CPU |
| --- | --- | --- |
| `mem_leaks` | 2.30 s | 2.28 s |
| `mem_heap` | 1.75 s | 1.73 s |
| `mem_vmmap` | 0.60 s | 0.58 s |
| `mem_history` | 0.73 s | 0.71 s |

The time grows with the app's heap. Reproduce on Settings (a bigger heap than the test app, so
slower): `python3 -I scripts/bench/ios-api.py --suite mem`; `mem_history` needs a leaked address
from the test app and has no script.

## Research measurements

These have their own methods in their own pages and aren't comparisons with the stock tools:
[research/software-mode-profile.md](research/software-mode-profile.md) (where software mode's
time goes, §2 reproduces it) and [research/netshape-overhead.md](research/netshape-overhead.md)
(the injected shaper's cost per call, its "Reproduce with" block). The mechanism numbers in
[android.md](android.md#performance-notes) are measured where they are stated.

## Numbers only in pull requests

Measured in a pull request and not carried into the docs; none is quoted anywhere. Each is the
PR's own before/after, with no script unless named:

- [#143](https://github.com/kaeawc/simee/pull/143): a screenshot through the socket as base64,
  binary and a `path` the server writes (8.8, 3.9, 1.5 ms). `scripts/ios-step-bench.py` times the
  base64 and `path` forms.
- [#138](https://github.com/kaeawc/simee/pull/138): `logs` with `stream: true`, 1.3 ms after the
  first call against ~760 ms.
- [#134](https://github.com/kaeawc/simee/pull/134): an in-process `simee tap` / `key` (307.5 /
  291.6 to 199.6 / 190.4 ms). `scripts/bench/ios-command.sh` times the in-process tap.
- [#132](https://github.com/kaeawc/simee/pull/132): `encode_png` before and after (6.26 to 2.91 ms
  at 2480x1740). `scripts/bench/host-paths.sh --only frame` prints both encoders.
- The first performance rounds, #9 to #16 and #26 to #28 (2026-11-23 and 24): superseded by the
  tables above where they overlap.

## Not yet measured

Claims that would belong in the README once someone runs them:

- Where a cold-cache boot's other 3.4 s over a warm one goes (renderer init is 1.2 s of its
  4.6 s): likely the guest's own pipelines compiling through MoltenVK, which
  `-provision-renderer`'s guest-less warm-up can't reach.
- Whether a farm's guest shaders, which share emu's per-bundle Metal cache (`dev.simee.emu`), push ANGLE's shader library out of it, so renderer init goes cold again on a busy farm.
- First boot of a fresh install with and without `emu -provision-renderer`'s renderer warm-up,
  launch to `sys.boot_completed` on a quiet host (only the renderer init itself is measured,
  at load 180 to 210). Each round needs a temporary `ANDROID_HOME` and so a 420.6 MB download of
  the emulator package (`-provision-renderer -dry-run`), which wasn't run without the owner's
  go-ahead; the host load was also 15 to 17, not under 16.

- First boot with and without interrupt kicks at a 1-minute load under 8: three more runs per arm (two `none` runs and three `targeted` runs are published), and the `targeted` default below macOS 26 has no measurement on that macOS.
- `simee screenshot` against `xcrun simctl io screenshot` on a quiet host (the run above had load
  13-69).
- iOS boot and settle on a quiet host (the runs above had other load).
- Time per automation step (tap, then screenshot) through the socket API against `adb` on the SDK
  emulator (`simctl` has no tap).
- Host Spotlight indexing of simulator data: `mdfind` lists 13,858 items under one booted-once
  device's directory (6,562 files), so the host's `mds_stores` indexes what devices write. What
  excluding `~/Library/Developer/CoreSimulator` from Spotlight saves a farm needs a change to the
  host's Spotlight privacy settings to measure.
- Host CPU of an MCP client subscribed to a booted device's `screen`, `logs` and `processes`
  resources, on iOS and Android, at the default pace and at `SIMEE_MCP_SCREEN_FPS=10`, against the
  same device read by polling `screenshot` from the client.
- Time per automation step through `simee mcp` (a `tools/call` of `tap`, then `screenshot`) against
  the socket API and the CLI on the same device.
- A 10 ms `button` hold for the iPhone's side, Siri, Apple Pay, Action and volume buttons and on
  Apple TV (the Siri Remote's keys) and Apple Watch, which all keep 80 ms.
- The iOS 10 ms tap and 3 ms key gap on what the test app doesn't have (measured only on a UIKit
  button, a SwiftUI button, a WKWebView link, a table row and a UITextField): Safari's toolbar and
  address bar, SpringBoard alerts, Control Center, the software keyboard's keys, UISwitch and
  UISlider, launching an app from its home-screen icon, and typing into a WKWebView field or a
  secure text field.
- iPad `pointer … --to` drag latency on a quiet host, and `pointer` timing and placement on
  iOS 26.5 since #122 (clicks on 27.2 are under "iPad `pointer` click").
- Host CPU of an idle booted device, emu against the SDK emulator, on API 31 and newer.
- API 30 warm boots against stock (first boots and idle host CPU of API 29 and 30 are
  [above](#api-29-and-30-first-boots-and-idle-host-cpu)), and API 29 and 30 idle host CPU after
  the AutoMobile harness.
- Network throughput and latency through emu's NAT against the SDK emulator's slirp.
- Host CPU of an idle booted device with and without the virtio-snd card (`EMU_VIRTIO_SND=0`),
  median of 3 boots each on the same host and day (an earlier sample on API 36 showed no
  difference, but its boots and command weren't recorded).
- A farm of SDK emulators on the same host, to compare with the emu farm above.
