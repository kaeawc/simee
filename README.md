# simee

<img src="assets/brand/simee-mark.svg" alt="The simee mark: a cut emerald" align="right" height="96">

iOS simulators and Android emulators without Simulator.app or the SDK emulator: one Rust core, one
CLI, one JSON socket API and one desktop window for both platforms. Your existing tools keep
working, because each device looks like the stock one: `adb`, `adb emu`, `simctl` and test
harnesses drive it unchanged.

![The desktop window with iOS and Android devices](docs/desktop.png)

## Performance

Against the stock tools on the same host and day, medians of 3 or more. Each simee cell reads
Android / iOS: Android against the SDK emulator (QEMU), iOS against Simulator.app. ↗ opens a
row's benchmark, Android's first; its tooltip names the host, date and script, which creates and
deletes its own devices.

| Measure | simee, Android / iOS | Android emulator (QEMU) | Simulator.app | Source |
| --- | --- | --- | --- | --- |
| Cold boot, a new device | **9.5 s** / 18.6 s | 22.9 s | 18.5 s | [↗](docs/benchmarks.md#android-emu-against-the-sdk-emulator "Android: M3 Max, 2026-11-24, scripts/bench/android-boot.sh") [↗](docs/research/ios-site-measures.md#results "iOS: M3 Max, 2026-11-27, loaded host, no script yet") |
| Warm boot | **3.5 s** / 6.6 s | 17.1 s | 6.6 s | [↗](docs/benchmarks.md#android-emu-against-the-sdk-emulator "Android: M3 Max, 2026-11-24, scripts/bench/android-boot.sh") [↗](docs/research/ios-site-measures.md#results "iOS: M3 Max, 2026-11-27, loaded host, no script yet") |
| Peak memory, warm | **2.09 GB** / 2.99 GB | 3.78 GB | 3.11 GB | [↗](docs/benchmarks.md#android-emu-against-the-sdk-emulator "Android: M3 Max, 2026-11-24, scripts/bench/android-boot.sh") [↗](docs/research/ios-site-measures.md#results "iOS: M3 Max, 2026-11-27, loaded host, no script yet") |
| Screenshot as a command, median | n/m / **23 ms** | n/m | 565 ms | [↗](docs/research/ios-site-measures.md#results "iOS: M3 Max, 2026-11-27, loaded host, no script yet") |
| Janky frames swiping the launcher, phone | **0.25%** / n/m | 6.2% | n/m | [↗](docs/benchmarks.md#frame-pacing "Android: M3 Max, 2026-11-25, scripts/bench/android-frames.sh") |
| Idle host CPU, of a core | **2.1%** / 13.6% (noisy) | 3.7% | **12.5%** (noisy) | [↗](docs/benchmarks.md#api-29-and-30-first-boots-and-idle-host-cpu "Android: M3 Max, 2026-11-26, scripts/bench/android-idle.sh") [↗](docs/research/ios-site-measures.md#results "iOS: M3 Max, 2026-11-27, loaded host, no script yet") |
| Warm boot, Linux without KVM | **168 s** / n/a | 2302 s | n/a | [↗](docs/benchmarks.md#boot-to-sysboot_completed-with-emus-devices "Android: 4-vCPU Xeon VM, 2026-11-26, scripts/bench/software-mode.sh") |

n/m: not measured. n/a: nothing to compare; iOS devices need a Mac. Bold marks the better of a
pair; a pair within 5% has none. A boot ends at `sys.boot_completed` on Android and when the boot
command returns on iOS.

On iOS simee is level with Simulator.app, not ahead. Its 6.6 s warm boot matches Simulator.app
with its window open, but a headless `simctl` boot on a device set of its own takes 2.6 s, and
simee can't boot a device in a custom device set yet, so it couldn't be timed that way. The iOS
idle CPU was measured under high host load (1-minute load 28-60): read it as indicative only. The
iOS screenshot is `simee screenshot` forwarded to `simee serve`; in its own process it takes
148 ms, and the p95s are 47 ms against 585 ms.

Android's idle CPU is API 30's; on API 29 it is 2.9% against 5.4%. On the Pixel Tablet 0% of
frames were janky against 5.6%: emu drew about 530 frames to stock's 1350, and on the phone both
drew about 790. The Linux row ran on a 4-vCPU Xeon VM with the stock emulator under `-accel off`,
where 1 of 3 boots never finished in 40 minutes. Every other row ran on an M3 Max: Android
2026-11-24 to 26, iOS 2026-11-27. `scripts/bench/run-all.sh --list` names every benchmark behind
the numbers on this page.

## Compared with the stock tools

### Android: emu against the SDK emulator

The same API 36 `google_apis` AVD (4 vCPU, 2 GB) under both, on one M3 Max, 2026-11-24, median
of 3 boots each. A cold boot is a new AVD's first; a warm boot is any later one, without a snapshot.

| | emu | SDK emulator |
| --- | --- | --- |
| Cold boot, until `sys.boot_completed` | **9.5 s** | 22.9 s |
| Warm boot, until `sys.boot_completed` | **3.5 s** | 17.1 s |
| Warm boot, until `adb` sees the device | **1.6 s** | 8.5 s |
| Peak memory, whole process tree, warm | **2.09 GB** | 3.78 GB |
| Janky frames swiping the launcher, Pixel Tablet / phone | **0% / 0.25%** | 5.6% / 6.2% |
| Java needed to provision an image | no | yes (`sdkmanager`) |

The SDK emulator restores a snapshot in 2.8 s; emu has no snapshots yet, and its warm boot is
within a second of that restore.

Guest code runs natively under both. The gap is the emulator itself: an
in-kernel interrupt controller, zero-copy disk and adb paths, and memory that is committed only
when the guest touches it.

### Android on Linux with KVM

On an x86_64 Linux host with a usable `/dev/kvm`, software mode runs the guest on KVM. On a GitHub
`ubuntu-latest` runner, 2026-11-27, the API 36 x86_64 image reached `sys.boot_completed` in **28.8 s**
cold and **16.6 s** warm, against the SDK emulator's 30.3 s and 19.2 s, with a 2.27 GB peak RSS
against 3.00 GB cold ([benchmarks](docs/benchmarks.md#android-software-mode-on-kvm-githubs-x86_64-runner)).

### Android on Linux without KVM

Software mode, for cloud and CI machines with no `/dev/kvm` and no GPU: QEMU's TCG runs the
guest's CPU and emu serves its devices. The API 36 x86_64 image on a 4-vCPU Xeon VM with 8 GiB,
2026-11-26 ([benchmarks](docs/benchmarks.md#android-software-mode-linux-without-kvm)).

| | emu | SDK emulator, `-accel off` |
| --- | --- | --- |
| Warm boot, until `sys.boot_completed`, median of 3 or more | **168 s** | 2302 s (1 of 3 never finished in 40 minutes) |
| Host CPU once idle | **0.75 cores** | 1.09 cores |
| Peak memory, QEMU and emu | 3.29 GB | **3.12 GB** |
| Tap / screenshot through the API, median | **60.8 ms / 36.9 ms** | no display socket; its framework kept restarting |

The SDK emulator runs every vCPU on one translator thread, and emu's QEMU runs one per vCPU.
Everything is drawn on the CPU, so the launcher draws about 7 frames a second.

### iOS: simee against Simulator.app and `simctl`

| | simee | Simulator.app and `simctl` |
| --- | --- | --- |
| Booting a device | headless, no app window | opens Simulator.app, or `simctl boot` with no screen |
| Screenshot as a command, median (loaded host) | **36 ms** through `simee serve`, 173 ms alone | 601 ms (`simctl io screenshot`) |
| Touch, keys, buttons, Digital Crown, Siri Remote | CLI and API | by hand in the window |
| Network conditions | per device, per host, live, no root | Network Link Conditioner: whole Mac, needs install |
| Audio | capture or mute one device, no driver | host output only |
| Processes, sockets, traffic per process | listed per device, signalable | Activity Monitor, across the whole Mac |
| App containers and preferences | browse, read, write, clear | find the path yourself |
| Core Animation overlays, memory warning, Face ID | scriptable | Debug menu, by hand |
| CarPlay display | attach, touch, screenshot | window only |

Every row on the left is a CLI command, an API method and a control in the desktop window.

How each number was measured, the `scripts/bench/` command that reruns it, and what has not been
measured yet: [docs/benchmarks.md](docs/benchmarks.md). The full comparison:
[docs/comparison.md](docs/comparison.md).

## Quick start

Needs macOS on Apple silicon, Rust (pinned in `rust-toolchain.toml`) and, for iOS, Xcode with a
simulator runtime. `simee doctor` says what this Mac can run and why.

| | Apple silicon (M1 and later) | Intel |
| --- | --- | --- |
| iOS devices | Xcode 26 or 27 (macOS 15.6+) | planned ([#241](https://github.com/kaeawc/simee/issues/241)) |
| iOS audio capture and mute | macOS 14.2+ | planned |
| Android devices (`emu`) | macOS 15+; 4 KiB guest memory pages on macOS 26+ | planned ([#242](https://github.com/kaeawc/simee/issues/242)) |

Tested on macOS 26; the floors and their sources: [docs/development.md](docs/development.md#support-matrix).

```bash
cargo build && alias simee=target/debug/simee

simee list                                   # every iOS and Android device
simee boot "iPhone 17"                       # headless; waits until it has booted
simee screenshot booted out.png
simee tap booted 600 1300
```

Android, with no Android Studio and no Java:

```bash
emu -provision 36 -tag google_apis            # platform-tools, the system image, the renderer
simee create Pixel --platform android --system-image "system-images;android-36;google_apis;arm64-v8a"
simee boot Pixel
adb -s emulator-5554 shell                    # an ordinary emulator to every tool
```

To put emu behind tools that launch `emulator` themselves: `make shim`, then
`export ANDROID_HOME=~/.simee/sdk`.

## What it covers

Phones, tablets, foldables, watches, TVs, cars and headsets on both platforms, each with its own
inputs (hinge, rotary, D-pad, Digital Crown, Siri Remote, vehicle properties, XR head pose).
iOS's foldable is the iPhone Duo. The matrix and its gaps:
[docs/form-factors.md](docs/form-factors.md).

## Not yet

- iOS needs an Apple silicon Mac (Intel: [#241](https://github.com/kaeawc/simee/issues/241)). Android
  runs on Apple silicon (Intel: [#242](https://github.com/kaeawc/simee/issues/242)), and on x86_64 Linux in software
  mode (x86_64 images; slow frames, see above). KVM on Linux is in progress
  ([#54](https://github.com/kaeawc/simee/issues/54)).
- No Android snapshots, so every boot is a full boot.
- AutoMobile's Android harness: 38 of 41 tools pass (2026-11-24). Open: network shaping and VM
  snapshots.
- Automotive and Android XR images have their devices implemented but have not been booted.
- iOS microphone injection still needs a loopback audio device.
- Android system images below API 29 are refused with the reason; emu hasn't been brought up on
  them.
- Android guests reach the internet over IPv4 only (TCP, UDP, ping); no IPv6 or inbound port
  forwarding yet. IP fragments the guest sends are dropped, and DNS goes to the host's first
  resolver, not to a VPN's scoped one.

## Documentation

| | |
| --- | --- |
| [Comparison](docs/comparison.md) | what differs from the stock tools, and why |
| [Benchmarks](docs/benchmarks.md) | every published number and the command behind it |
| [CLI](docs/cli.md) and [API](docs/api.md) | commands and methods for both platforms |
| [MCP server](docs/mcp.md) | `simee mcp`: every method as a tool for an agent, with resources, tasks and a device view |
| [Debugging](docs/debugging.md) | the iOS debug surface, network shaping, audio, Android counters |
| [Desktop window](docs/desktop.md) | the window, its shortcuts and per-device controls |
| [Android backend](docs/android.md) | emu status, provisioning, performance notes |
| [Architecture](docs/architecture.md) | layers, process model, design decisions |
| [Development](docs/development.md) | requirements, checks against real devices |

## License

MIT
