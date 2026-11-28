# Changelog

Notable changes to simee. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/). Add entries under
`## [Unreleased]`; `scripts/release.sh` moves them under the new version.

## [Unreleased]

### Added

- simee's mark, a cut emerald, drawn from one description in `sim_core::mark` (its cut, light,
  colours and motion states): the macOS app icon (`.icns`), favicon, menu bar template, PNGs and
  SVGs in `assets/brand/` (`scripts/brand.sh` redraws them), and in the desktop window the
  activity indicator beside a running job, each device's state in the list and the empty stage.
  `scripts/desktop-app.sh` bundles the window as `simee.app` with its icon.

### Changed

- The iPhone Duo is iOS's foldable: `form_factors` lists (iOS, foldable) as supported with the
  Duo's reference profile (2853×2007 unfolded, 1398×2034 folded, @3x), its devices and device type
  report `form_factor: foldable`, and `create --form-factor foldable` takes an iPhone Duo. Rotation,
  CarPlay's form factor check and the desktop's edge gestures treat it as the iPhone it is.

### Fixed

- `simee-rtc` on macOS: a WHIP server that replies and then resets the connection (an early
  error, or a request body it didn't read) no longer fails the request with "Invalid argument";
  its reply is read.

## [0.2.0] - 2026-11-27

### Added

- `simee-host`, a new crate: a `host_health` method reporting load, memory pressure, thermal
  state, power source and the host's power preferences, with an admit/hold/backoff verdict
  (#273); and `devices_usage`, which attributes CPU, memory, I/O and energy to every running
  device and to simee itself (#274).
- `simee-rtc`, a new crate: a WebRTC publisher written in Rust on str0m that sends H.264 over
  WHIP, and feeds PLI and bitrate requests back to the encoder (#264).
- `simee-encode`, a new crate: an H.264 encoder on macOS VideoToolbox that takes device frames
  directly. iOS frames are encoded with no copy, and Android frames are copied once. Encoding is
  paced by the device's own frames, keyframes are sent on request, and output is Annex B (#263;
  there is no Linux encoder yet).
- macOS support matrix: each feature has a minimum macOS version, simee gives a clear error below
  it, and `simee doctor` reports where the host stands (#238).
- Desktop: orbit, pan and zoom the 3D device view, a 1:1 face-on view, saved views, and flip and
  rotate, all eased (#155).

### Changed

- MCP: every tool declares an output schema, returns `structuredContent`, and carries
  annotations taken from sim-api's catalog. Tools are grouped into toolsets: the default `core`
  set serves 22 tools, and `SIMEE_MCP_TOOLSETS=all` restores the full list. `processes`, `logs`,
  `connections` and `devices_usage` take `limit` and `cursor` for paging. Every default the
  schemas advertise now matches what the server does when the key is omitted (#292, #223).
- `emu` no longer refuses to load on macOS versions before 26 (#238).

### Fixed

- `contrast_check`: a card's drop shadow no longer hides failing text inside the card (#233).
- Android: two screenshots of one device taken at the same moment no longer overwrite each
  other's temporary file (#311).

## [0.1.0] - 2026-11-27

The first release: iOS simulator and Android emulator devices from one Rust core, published to
crates.io as `simee` (the CLI), `simee-emu` (the Android emulator) and the `simee-*` library
crates they are built from.

### Added

- `simee`, one CLI, JSON-lines socket API (`simee serve`) and MCP server (`simee mcp`) for both
  platforms: create, boot, shut down and delete devices, install and launch apps, screenshots and
  frame streams, touch, keys, buttons and text input, rotation and form factors (phones, tablets,
  foldables, watches, TVs, cars, XR), network shaping without root, per-device audio capture,
  processes, sockets, app containers and preferences, and memory tools.
- iOS: devices driven through the host's simulator service directly, headless, with no
  Simulator.app window; Xcode 26 and 27.
- Android: `emu`, simee's own emulator on Hypervisor.framework (Apple silicon) that boots
  unmodified SDK system images with the stock board, ports, console and `adb`, provisions images
  without Java (`emu -provision`), and runs on Linux in software mode (QEMU's TCG with emu's
  devices) where there is no KVM.
- A desktop window (Rune) for live, interactive devices of both platforms (built from `desktop/`,
  not published to crates.io).
