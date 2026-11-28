# Architecture

simee runs iOS simulator devices and Android emulator devices behind one core: one device model,
one method table, one CLI, one desktop window. This page explains how it is put together and why,
including where the two projects it grew from (sim-rs and emu-rs, together renamed simee) disagreed.

## Layers

| Layer | Crates | Builds on | What it does |
| --- | --- | --- | --- |
| Shared core | `sim-core` | everywhere | Pure logic, unit-tested: devices and selection, platforms and form factors, frames and PNG, pixel formats, gestures, keys and buttons, the display stream protocol |
| Control surface | `sim-api` | everywhere | The method table and dispatcher, the JSON-lines socket server and client |
| iOS backend | `sim-coresim`, `sim-debug`, `sim-netshape`, `sim-audio` | macOS (`sim-coresim` compiles to nothing elsewhere) | CoreSimulator through the Objective-C runtime, HID input, the IOSurface framebuffer; host-side debugging, network shaping, audio |
| Host health | `sim-host` | everywhere (macOS reads sysctls, `NSProcessInfo` and IOKit through `sim-coresim`; Linux reads `/proc` and `/sys`) | Load, memory pressure, swap, disk, thermal state and power, a 15-minute ring, and the admit/hold/backoff verdict (thresholds ported from spectra); per-device CPU, memory, disk I/O and energy from one libproc pass per sample (`usage`, macOS; attribution rules ported from spectra) |
| Streaming | `sim-rtc`, `sim-encode` | everywhere (the DTLS backend is CryptoKit on Apple targets, opt-in RustCrypto on Linux; the encoder is VideoToolbox, macOS only for now) | The WHIP publisher: H.264 over RTP on str0m (sans-IO WebRTC), keyframe requests and bandwidth estimates for the encoder; the encoder: H.264 straight from the iOS framebuffer IOSurface or emu's frame ring, paced by the device's frames ([streaming.md](streaming.md)) |
| Android backend, client | `sim-android` | everywhere | AVDs as devices, starting and stopping `emu`, the console client and the display-socket client |
| Android backend, emulator | `apps/emu`, `emu-vmm`, `emu-hvf`, `emu-gfxstream`, `emu-avd`, `emu-sdk`, `emu-ext4`, `emu-fdt`, `emu-container` | VMM on Apple silicon Macs; the rest everywhere | The emulator itself: Hypervisor.framework VM, stock board, virtio devices, gfxstream renderer, console and display servers, AVD files, Java-free provisioning |
| Front ends | `apps/simee`, `desktop/` | everywhere (devices only on macOS) | The CLI and the Rune window, both clients of `sim-api` |

## Process model

```text
 simee CLI ─┐                         ┌─ CoreSimulator ─ launchd_sim (one per iOS device)
 desktop ────┼─ sim-api (in process) ──┤
 simee serve┘   └─ JSON lines socket  └─ emu (one process per Android device)
                                              ├─ console port (stock text protocol)
                                              ├─ <avd>/simee/display.sock (sim_core::stream)
                                              │    └─ frames-<pid>-<n>.ring (shared frame ring)
                                              └─ adb bridge (console port + 1)
```

Every device runs outside the front-end processes. iOS devices always did (CoreSimulator runs them
under `launchd_sim`); for Android it's both forced and wanted:

- Hypervisor.framework allows one VM per process, so several Android devices need several
  processes anyway.
- A wedged or crashed guest can't take the window or the API server down.
- Tools that already speak to the stock emulator (`adb`, `adb emu`, AutoMobile) keep working,
  because `emu` keeps the stock console and advertises itself where the SDK tools look
  (`~/Library/Caches/TemporaryItems/avd/running/pid_<pid>.ini`).

The front ends drive an Android device the way any tool can: `sim-android` sends console commands
(input events, sensors, location, rotation, the hinge, screenshots, `kill`) and streams frames and
touches over the display socket. It never links the VMM.

## One device model

`sim_core::device::DeviceInfo` describes devices of both platforms:

| Field | iOS | Android |
| --- | --- | --- |
| `udid` | the simulator's UDID | the AVD name (Android has no other stable id for a stopped device) |
| `runtime_id` | `com.apple.CoreSimulator.SimRuntime.iOS-26-5` | `system-images;android-36;google_apis;arm64-v8a` |
| `device_type_id` | `com.apple.CoreSimulator.SimDeviceType.iPhone-17` | `hw.device.name`, else `custom` |
| `state` | the service's state | `Booted` while an emulator advertises it, else `Shutdown` |
| `platform` | `ios` | `android` |
| `form_factor`, `sub_platform` | from the device type and runtime | from the AVD config and image tag |

Device selection (`select`: UDID, then `booted`, then name) is shared. The API resolves a `device`
parameter across both platforms (or one, with `platform`), then runs the method on that device's
backend. When there are no Android devices the query goes straight to the iOS backend, exactly as
before the merge.

## Platforms and form factors

`sim_core::platform` and `sim_core::form_factor` are the single source of truth for labels and
support: `Kind::new(platform, form_factor)` either returns the sub-platform or an `Unsupported`
with the reason. Profiles give each kind a reference screen, density, shape, fold state and input
methods. The API (`form_factors`, `devices` filters, `create` validation), the CLI
(`form-factors`, `list` labels) and the window (badges, toolbars, round screens) all read from it.
See [form-factors.md](form-factors.md).

## Shared logic, and where it came from

| Shared piece | Lives in | Replaced |
| --- | --- | --- |
| `Frame`, PNG encoding | `sim_core::frame` | sim-rs's encoder in `sim-api`; emu-rs's hand-rolled `png.rs` |
| Pixel formats, downscaling | `sim_core::pixels` | sim-rs's BGRA conversion; emu-rs's virtio-gpu conversion and `downscale` |
| Display stream protocol | `sim_core::stream` | the header and line parsing in emu-rs's display server and desktop client |
| Keys and buttons | `sim_core::hid` | emu-rs's lowercase-only `event text`; per-platform button lists |
| Gestures | `sim_core::gesture` | (sim-rs's, now used by Android taps and swipes too) |
| Device model, selection | `sim_core::device` | emu-rs's desktop `AvdRow` |
| Running instances | `emu_avd::Running` | two parsers (console and desktop) |
| Method table | `sim-api` | sim-rs's CLI's own copies of input and boot logic |

## Decisions where the two codebases conflicted

| Conflict | sim-rs had | emu-rs had | Chosen |
| --- | --- | --- | --- |
| `unsafe` lint | `forbid` everywhere but its FFI crate | `deny`, with one opt-in in `apps/emu` | `forbid`; `apps/emu` keeps `deny` locally (its signal-mask function) |
| Dev profile | deps unoptimized except PNG | deps at opt-level 2, VMM crates at 1 | emu-rs's: the VMM is the guest's hot path |
| Cargo runner | none | signs every binary with the hypervisor entitlement | kept; harmless for binaries that don't use it |
| CI runner | macos-26 | macos-15 (failed to link `hv_vm_config_set_ipa_granule`) | macos-26, which links |
| Control surface | JSON method table | stock text console | both: the console stays for stock tools, the method table drives Android through it |
| Desktop process model | in-process service connection | out-of-process emulator | out of process for Android (one VM per process; crash isolation) |
| Boot readiness | a framebuffer exists | `sys.boot_completed` | `sys.boot_completed` through adb when available, else the first frame |
| Debug panel | host-side process, socket and storage views | guest console pane | each platform keeps its own panel |
| Crate names | `sim-*` | `emu-*` | kept, so the port maps onto upstream; AGENTS.md says which is which |

## Where each part is verified

| Check | Runs on | Covers |
| --- | --- | --- |
| `cargo clippy` + `cargo test` (CI `linux`) | Linux | the shared core, the API (both backends' dispatch), the CLI, `sim-android` against a fake emulator, emu-avd/ext4/fdt/sdk |
| `cargo clippy --target aarch64-apple-darwin` | any host | type-checks and lints the macOS-only code without linking |
| CI `lint-test` | macOS 26 | everything above plus the iOS backend and the VMM's unit tests (modem, virtio, ...), and links the release `emu` |
| CI `desktop`, `desktop-linux` | macOS, Linux | the window's headless view tests |
| `SIMEE_E2E_UDID`, `scripts/parity.sh`, the iOS AutoMobile harness | a Mac with a booted simulator | real iOS devices |
| `SIMEE_E2E_AVD`, booting an AVD, the Android AutoMobile harness | an Apple silicon Mac | real Android devices (hosted runners can't nest the hypervisor) |
